use std::net::IpAddr;
use std::time::Instant;

use crate::board::Board;
use crate::convergence::Convergence;
use crate::model::{Client, Hop, Host, Route, Upload};

#[test]
#[ignore = "component load test"]
fn board_and_convergence_load() {
    for count in [1, 64, 4096, 65_536] {
        let targets = vec!["x".repeat(253); count];
        let board = Board::new(&targets);
        let started = Instant::now();
        for _ in 0..20 {
            assert_eq!(board.snapshot().rows.len(), count);
        }
        eprintln!("board: {count} rows, 20 snapshots: {:?}", started.elapsed());
    }
    let mut convergence = Convergence::default();
    let responders: Vec<_> = (1..=254)
        .flat_map(|ttl| (1..=16).map(move |host| (ttl, IpAddr::from([192, 0, ttl, host]))))
        .collect();
    let started = Instant::now();
    for round in 1..=2000 {
        convergence.observe(round, responders.iter().copied());
    }
    assert!(convergence.is_stable(5, true));
    eprintln!(
        "convergence: 2000 rounds, 4064 responders: {:?}",
        started.elapsed()
    );
}

#[test]
#[ignore = "component load test"]
fn large_route_output_and_serialization() {
    let route = Route {
        target: "long-target.example".to_owned(),
        destination: "2001:db8::1".to_owned(),
        protocol: "icmp",
        family: 6,
        started_at: "2026-10-29T00:00:00Z".to_owned(),
        duration_ms: 65_535_000,
        rounds: 65_535,
        reached: false,
        hops: (1..=254)
            .map(|ttl| Hop {
                ttl,
                hosts: (1..=8)
                    .map(|host| Host {
                        ip: format!("2001:db8::{ttl:x}:{host:x}"),
                        hostname: Some(format!("{}.example", "h".repeat(240))),
                        asn: Some(u32::MAX),
                        replies: 65_535,
                    })
                    .collect(),
                sent: 65_535,
                recv: 65_535,
                loss_pct: 0.0,
                last_ms: Some(1000.0),
                avg_ms: Some(1000.0),
                best_ms: Some(999.0),
                worst_ms: Some(1001.0),
                stdev_ms: Some(0.5),
                samples_ms: vec![1000.0; 64],
            })
            .collect(),
    };
    let started = Instant::now();
    let text = crate::output::render(&route);
    assert!(text.contains("AS4294967295"));
    eprintln!(
        "table: 254 hops, 8 hosts per hop, {} bytes: {:?}",
        text.len(),
        started.elapsed()
    );
    let body = Upload {
        schema: 1,
        client: Client::current(),
        share_origin: false,
        pin: None,
        routes: vec![route; 64],
    };
    let started = Instant::now();
    let json = serde_json::to_vec(&body).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["routes"].as_array().unwrap().len(), 64);
    eprintln!(
        "JSON: 64 routes, {} bytes, encode and decode: {:?}",
        json.len(),
        started.elapsed()
    );
}
