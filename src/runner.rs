use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow};
use trippy_core::{Builder, Port, PortDirection, PrivilegeMode, Protocol, State};

use crate::board::{Board, Stage};
use crate::convergence::Convergence;
use crate::model;
use crate::plan::Job;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SAMPLES: usize = 64;
const UDP_SOURCE_PORT_BASE: u16 = 41000;
const SEQUENCE_BLOCK: u16 = 1536;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub privilege: PrivilegeMode,
    pub min_rounds: usize,
    pub max_rounds: usize,
    pub time_limit: Duration,
    pub interval: Duration,
    pub max_ttl: u8,
    pub tcp_port: u16,
}

#[derive(Debug)]
pub struct Outcome {
    pub label: String,
    pub result: Result<model::Route>,
}

pub const fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Icmp => "icmp",
        Protocol::Udp => "udp",
        Protocol::Tcp => "tcp",
    }
}

fn job_label(job: &Job, protocol: Protocol) -> String {
    let family = if job.addr.is_ipv4() { "IPv4" } else { "IPv6" };
    if job.target == job.addr.to_string() {
        format!(
            "{} {family}/{}",
            job.target,
            protocol_name(protocol).to_uppercase()
        )
    } else {
        format!(
            "{} ({}) {family}/{}",
            job.target,
            job.addr,
            protocol_name(protocol).to_uppercase()
        )
    }
}

/// Rounds the tracer itself runs. trippy cannot be stopped from outside, so a tracer that outlives its
/// route keeps probing until this many rounds; capping it at what fits in the time limit keeps such
/// leftovers short.
fn tracer_rounds(settings: &Settings) -> usize {
    let interval = settings.interval.max(Duration::from_millis(100));
    let fit = settings.time_limit.as_nanos().div_ceil(interval.as_nanos()) + 1;
    settings
        .max_rounds
        .min(usize::try_from(fit).unwrap_or(usize::MAX))
}

/// Every concurrent tracer gets its own ICMP identifier, UDP source port and sequence range,
/// so replies are never attributed to the wrong route.
fn build_tracer(
    job: &Job,
    protocol: Protocol,
    slot: u16,
    settings: &Settings,
) -> Result<trippy_core::Tracer> {
    let identifier = u16::try_from(std::process::id() % u32::from(u16::MAX))
        .unwrap_or(1)
        .wrapping_mul(97)
        .wrapping_add(slot.wrapping_mul(13))
        .max(1);
    let port_direction = match protocol {
        Protocol::Icmp => PortDirection::None,
        Protocol::Udp => PortDirection::FixedSrc(Port(UDP_SOURCE_PORT_BASE + slot)),
        Protocol::Tcp => PortDirection::FixedDest(Port(settings.tcp_port)),
    };
    Builder::new(job.addr)
        .privilege_mode(settings.privilege)
        .protocol(protocol)
        .trace_identifier(identifier)
        .port_direction(port_direction)
        .initial_sequence(33434 + (slot % 20) * SEQUENCE_BLOCK)
        .max_rounds(Some(tracer_rounds(settings)))
        .max_ttl(settings.max_ttl)
        .min_round_duration(settings.interval)
        .max_round_duration(settings.interval.max(Duration::from_millis(1000)))
        .build()
        .map_err(|error| anyhow!("{error}"))
}

fn responders(state: &State) -> Vec<(u8, IpAddr)> {
    state
        .hops()
        .iter()
        .flat_map(|hop| hop.addrs().map(move |addr| (hop.ttl(), *addr)))
        .collect()
}

fn reached(state: &State, target: IpAddr) -> bool {
    state
        .hops()
        .iter()
        .any(|hop| hop.addrs().any(|addr| *addr == target))
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Converts trippy's per-hop state into the upload model, stopping at the hop that answered for the target.
fn route_from_state(
    job: &Job,
    protocol: Protocol,
    state: &State,
    rounds: usize,
    started: SystemTime,
    elapsed: Duration,
) -> model::Route {
    let target_ttl = state
        .hops()
        .iter()
        .find(|hop| hop.addrs().any(|addr| *addr == job.addr))
        .map(trippy_core::Hop::ttl);
    let hops = state
        .hops()
        .iter()
        .filter(|hop| hop.total_sent() > 0 && target_ttl.is_none_or(|ttl| hop.ttl() <= ttl))
        .map(|hop| {
            let answered = hop.total_recv() > 0;
            let mut samples: Vec<f64> = hop
                .samples()
                .iter()
                .filter(|sample| !sample.is_zero())
                .take(MAX_SAMPLES)
                .map(|sample| round_ms(sample.as_secs_f64() * 1000.0))
                .collect();
            samples.reverse();
            model::Hop {
                ttl: hop.ttl(),
                hosts: hop
                    .addrs_with_counts()
                    .map(|(addr, count)| model::Host {
                        ip: addr.to_string(),
                        hostname: None,
                        asn: None,
                        replies: *count,
                    })
                    .collect(),
                sent: hop.total_sent(),
                recv: hop.total_recv(),
                loss_pct: (hop.loss_pct() * 100.0).round() / 100.0,
                last_ms: hop.last_ms().map(round_ms),
                avg_ms: answered.then(|| round_ms(hop.avg_ms())),
                best_ms: hop.best_ms().map(round_ms),
                worst_ms: hop.worst_ms().map(round_ms),
                stdev_ms: answered.then(|| round_ms(hop.stddev_ms())),
                samples_ms: samples,
            }
        })
        .collect();
    model::Route {
        target: job.target.clone(),
        destination: job.addr.to_string(),
        protocol: protocol_name(protocol),
        family: if job.addr.is_ipv4() { 4 } else { 6 },
        started_at: humantime::format_rfc3339_millis(started).to_string(),
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        rounds,
        reached: target_ttl.is_some(),
        hops,
    }
}

/// Runs one tracer until the path is stable, the round limit or the time limit is hit.
fn trace(
    job: &Job,
    protocol: Protocol,
    slot: u16,
    settings: &Settings,
    board: &Board,
    index: usize,
) -> Result<model::Route> {
    let started = SystemTime::now();
    let clock = Instant::now();
    let (tracer, handle) = build_tracer(job, protocol, slot, settings)?
        .spawn()
        .map_err(|error| anyhow!("{error}"))?;
    let flow = State::default_flow_id();
    let mut convergence = Convergence::default();

    loop {
        thread::sleep(POLL_INTERVAL);
        let state = tracer.snapshot();
        if let Some(error) = state.error() {
            return Err(anyhow!("{error}"));
        }
        let rounds = state.round_count(flow);
        let is_reached = reached(&state, job.addr);
        convergence.observe(rounds, responders(&state));
        board.update(index, |row| {
            row.rounds = rounds;
            row.reached = is_reached;
        });
        let finished = handle.is_finished();
        if finished
            || convergence.is_stable(settings.min_rounds, is_reached)
            || clock.elapsed() >= settings.time_limit
        {
            if finished {
                handle
                    .join()
                    .map_err(|_| anyhow!("tracer thread panicked"))?
                    .map_err(|error| anyhow!("{error}"))?;
            }
            // A tracer that is still running ends on its own after `tracer_rounds` rounds.
            let state = tracer.snapshot();
            return Ok(route_from_state(
                job,
                protocol,
                &state,
                state.round_count(flow),
                started,
                clock.elapsed(),
            ));
        }
    }
}

/// Traces all jobs with up to `parallel` tracers at once. A job whose first protocol does not reach the
/// target is retried with its fallback protocol on the same worker, and both routes are kept.
pub fn run_all(jobs: &[Job], parallel: usize, settings: &Settings, board: &Board) -> Vec<Outcome> {
    let next = AtomicUsize::new(0);
    let slots = AtomicUsize::new(0);
    let outcomes = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for _ in 0..parallel.min(jobs.len()).max(1) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = jobs.get(index) else { break };
                    let job_started = Instant::now();
                    let mut protocol = Some(job.protocol);
                    let mut first = true;
                    while let Some(current) = protocol.take() {
                        let slot =
                            u16::try_from(slots.fetch_add(1, Ordering::SeqCst) % 1000).unwrap_or(0);
                        let label = job_label(job, current);
                        board.update(index, |row| {
                            row.protocol = current;
                            row.fallback = !first;
                            row.stage = Stage::Tracing;
                            row.rounds = 0;
                            row.reached = false;
                            row.started = Some(Instant::now());
                        });
                        let result = trace(job, current, slot, settings, board, index);
                        let retry = first && matches!(&result, Ok(route) if !route.reached);
                        if let Ok(mut list) = outcomes.lock() {
                            list.push(Outcome { label, result });
                        }
                        if retry {
                            protocol = job.fallback;
                        }
                        first = false;
                    }
                    board.update(index, |row| {
                        row.stage = Stage::Done;
                        row.elapsed = job_started.elapsed();
                    });
                }
            });
        }
    });
    outcomes.into_inner().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(time_limit: Duration, interval: Duration) -> Settings {
        Settings {
            privilege: PrivilegeMode::Privileged,
            min_rounds: 5,
            max_rounds: 30,
            time_limit,
            interval,
            max_ttl: 30,
            tcp_port: 443,
        }
    }

    #[test]
    fn round_budget_keeps_the_partial_round_and_final_poll() {
        for (limit, interval, expected) in [
            (Duration::from_secs(45), Duration::from_secs(1), 30),
            (Duration::from_secs(1), Duration::from_millis(100), 11),
            (Duration::from_millis(1001), Duration::from_millis(100), 12),
            (Duration::from_secs(1), Duration::from_secs(2), 2),
        ] {
            assert_eq!(tracer_rounds(&settings(limit, interval)), expected);
        }
    }

    #[test]
    fn round_budget_caps_durations_larger_than_usize_rounds() {
        for limit in [
            Duration::from_secs(u64::from(u32::MAX)),
            Duration::from_secs(u64::MAX),
            Duration::MAX,
        ] {
            assert_eq!(
                tracer_rounds(&settings(limit, Duration::from_millis(100))),
                30
            );
        }
        assert_eq!(tracer_rounds(&settings(Duration::MAX, Duration::MAX)), 2);
    }
}
