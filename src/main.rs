mod board;
mod cli;
mod convergence;
mod dns;
mod live;
mod manage;
mod model;
mod output;
mod plan;
mod runner;
mod upload;

#[cfg(test)]
mod stress;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use ratatui::crossterm::style::Stylize;

use crate::board::Board;
use crate::cli::{Cli, Command, SelfCommand};
use crate::dns::Dns;
use crate::live::Live;
use crate::runner::Settings;

const DEFAULT_PARALLEL: usize = 8;

/// True when no hop of the route answered a single probe.
fn silent(route: &model::Route) -> bool {
    route.hops.iter().all(|hop| hop.recv == 0)
}

/// Everything a run produced, printed once the live view is gone.
struct Report {
    skipped: Vec<String>,
    failures: Vec<String>,
    unanswered: usize,
    body: model::Upload,
    upload: Option<Result<model::UploadAccepted>>,
}

fn settings(cli: &Cli, privilege: trippy_core::PrivilegeMode) -> Settings {
    Settings {
        privilege,
        min_rounds: usize::from(cli.min_rounds.min(cli.max_rounds)),
        max_rounds: usize::from(cli.max_rounds),
        time_limit: cli.time_limit,
        interval: cli.interval,
        max_ttl: cli.max_ttl,
        tcp_port: cli.port,
    }
}

/// Resolves, traces, annotates and uploads, recording each step on `board`.
fn work(cli: &Cli, board: &Board) -> Result<Report> {
    let dns = Dns::new(&cli.dns)?;
    let plan = plan::build(cli, &dns)?;
    board.start_trace(&plan.jobs);
    let parallel = cli
        .parallel
        .map_or(DEFAULT_PARALLEL.min(plan.jobs.len()), usize::from);
    let outcomes = runner::run_all(&plan.jobs, parallel, &settings(cli, plan.privilege), board);

    let mut routes = Vec::new();
    let mut failures = Vec::new();
    for outcome in outcomes {
        match outcome.result {
            Ok(route) if silent(&route) => {
                failures.push(format!("{}: no probe was answered", outcome.label));
            }
            Ok(route) => routes.push(route),
            Err(error) => failures.push(format!("{}: {error:#}", outcome.label)),
        }
    }
    routes.sort_by_key(|route| (route.target.clone(), route.family, !route.reached));

    let mut unanswered = 0;
    if !cli.no_rdns && !routes.is_empty() {
        board.start_lookup();
        unanswered = dns.annotate(&mut routes, |answered, total| {
            board.set_lookup(answered, total);
        });
    }
    let body = model::Upload {
        schema: 1,
        client: model::Client::current(),
        share_origin: !cli.hide_origin,
        pin: cli.pin.clone(),
        routes,
    };
    let upload = (!cli.no_upload && !body.routes.is_empty()).then(|| {
        board.start_upload();
        upload::upload(&cli.api, &body, &dns)
    });
    Ok(Report {
        skipped: plan.skipped,
        failures,
        unanswered,
        body,
        upload,
    })
}

fn report(cli: &Cli, report: Report) -> Result<ExitCode> {
    if !cli.quiet {
        for skipped in &report.skipped {
            eprintln!("skip {skipped}");
        }
    }
    for failure in &report.failures {
        eprintln!("error {failure}");
    }
    if report.body.routes.is_empty() {
        anyhow::bail!("no route could be traced");
    }
    if report.unanswered > 0 && !cli.quiet {
        eprintln!(
            "note: no reverse DNS or AS answer in time for {} addresses",
            report.unanswered
        );
    }
    // With --json the tables go to stderr so stdout holds only the JSON document.
    if !cli.quiet {
        for route in &report.body.routes {
            if cli.json {
                eprintln!("{}\n", output::render(route));
            } else {
                println!("{}\n", output::render(route));
            }
        }
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report.body)?);
    }
    match report.upload {
        None => Ok(ExitCode::SUCCESS),
        Some(Ok(accepted)) => {
            if cli.quiet {
                println!("{}", accepted.url);
            } else {
                println!("trace-id {}\n{}", accepted.id, accepted.url);
                if accepted.pin {
                    println!("Readers also need the PIN you set.");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Err(error)) => {
            eprintln!("error: {error:#}");
            Ok(ExitCode::from(2))
        }
    }
}

fn live_options(cli: &Cli) -> Result<live::Options> {
    let upload = if cli.no_upload {
        None
    } else {
        let uri: ureq::http::Uri = cli
            .api
            .parse()
            .with_context(|| format!("reading the gateway address {}", cli.api))?;
        Some(uri.host().unwrap_or(&cli.api).to_owned())
    };
    Ok(live::Options {
        lookup: !cli.no_rdns,
        upload,
        min_rounds: usize::from(cli.min_rounds.min(cli.max_rounds)),
        time_limit: cli.time_limit,
    })
}

/// Hands the report to the live view, which prints it as the result panel.
fn show_result(cli: &Cli, live: Live, report: Report) -> Result<ExitCode> {
    let (upload, code) = match report.upload {
        None => (live::Upload::Skipped, ExitCode::SUCCESS),
        Some(Ok(accepted)) => (live::Upload::Accepted(accepted), ExitCode::SUCCESS),
        Some(Err(error)) => (
            live::Upload::Failed(format!("{error:#}")),
            ExitCode::from(2),
        ),
    };
    let link_below = live.finish(Some(live::Summary {
        routes: report.body.routes.clone(),
        upload,
        skipped: report.skipped,
        failures: report.failures,
        unanswered: report.unanswered,
    }))?;
    if let Some(link) = link_below {
        // Plain text the terminal can wrap, beside the panel on stderr.
        eprintln!("{}", link.as_str().cyan().underlined());
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report.body)?);
    }
    Ok(code)
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let board = Arc::new(Board::new(&cli.targets));
    let live = if !cli.quiet && std::io::stderr().is_terminal() {
        match Live::start(Arc::clone(&board), live_options(cli)?) {
            Ok(live) => Some(live),
            Err(error) => {
                eprintln!("note: showing results without the live panel: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let result = work(cli, &board);
    let Some(live) = live else {
        return report(cli, result?);
    };
    // The result panel goes to stderr, so it replaces the tables only where they would have been
    // shown in the same terminal; `--json` already puts them on stderr.
    let in_panel = cli.json || std::io::stdout().is_terminal();
    match result {
        Ok(report) if in_panel && !report.body.routes.is_empty() => show_result(cli, live, report),
        result => {
            live.finish(None)?;
            report(cli, result?)
        }
    }
}

fn main() -> ExitCode {
    // Package managers check a command-line tool by starting it without arguments, so a bare `xmtr`
    // prints the help and succeeds.
    if std::env::args_os().len() == 1 {
        return match Cli::command().print_help() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let cli = Cli::parse();
    let result = match &cli.command {
        Some(Command::Manage(SelfCommand::Update { force, tag })) => {
            manage::update(*force, tag.as_deref()).map(|()| ExitCode::SUCCESS)
        }
        Some(Command::Manage(SelfCommand::Uninstall)) => {
            manage::uninstall().map(|()| ExitCode::SUCCESS)
        }
        None => run(&cli),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
