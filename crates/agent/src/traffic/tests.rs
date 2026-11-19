use super::config::{ReportMode, Weights};
use super::ledger::{Interface, LIMIT, Snapshot, Totals};
use super::*;
use std::collections::BTreeMap;

fn cfg() -> TrafficConfig {
    TrafficConfig {
        enabled: true,
        include: vec!["P".into(), "R".into()],
        ..TrafficConfig::default()
    }
}
fn snapshot(p: (u64, u64), r: (u64, u64)) -> Snapshot {
    Snapshot {
        boot: "boot-1:netns-1".into(),
        interfaces: BTreeMap::from([
            (
                "P".into(),
                Interface {
                    identity: "1:1:1:mac-p".into(),
                    rx: p.0,
                    tx: p.1,
                },
            ),
            (
                "R".into(),
                Interface {
                    identity: "2:2:1:mac-r".into(),
                    rx: r.0,
                    tx: r.1,
                },
            ),
        ]),
    }
}
fn japanese() -> TrafficConfig {
    let mut c = cfg();
    c.weights.insert(
        "P".into(),
        Weights {
            rx_to_in_ppm: 0,
            tx_to_out_ppm: 1_000_000,
            ..Weights::default()
        },
    );
    c.weights.insert(
        "R".into(),
        Weights {
            rx_to_in_ppm: 0,
            rx_to_out_ppm: 1_000_000,
            tx_to_out_ppm: 2_000_000,
            ..Weights::default()
        },
    );
    c
}

#[test]
fn default_disabled_and_plain_raw_compatibility() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("unused");
    let config: crate::config::AgentConfig = serde_json::from_value(serde_json::json!({
        "server_url": "http://127.0.0.1:1",
        "collector": { "traffic": { "state_dir": path } }
    }))
    .unwrap();
    assert!(!config.collector.traffic.enabled);
    assert!(Monitor::start(&config.collector.traffic).unwrap().is_none());
    assert!(!path.exists());
    let c = cfg();
    let mut state = State::seed(&c, &snapshot((100, 200), (300, 400)), Totals::default()).unwrap();
    assert_eq!(
        state
            .advance(&c, &snapshot((110, 220), (330, 440)))
            .unwrap(),
        (40, 60)
    );
    assert_eq!(state.totals.raw_in, 40);
    assert_eq!(state.totals.billing_in, 40);
    assert_eq!(state.totals.raw_out, 60);
    assert_eq!(state.totals.billing_out, 60);
}

#[test]
fn japanese_and_all_four_half_direction_mapping() {
    let c = japanese();
    let mut state = State::seed(&c, &snapshot((0, 0), (0, 0)), Totals::default()).unwrap();
    state.advance(&c, &snapshot((10, 20), (30, 40))).unwrap();
    assert_eq!(
        (state.totals.billing_in, state.totals.billing_out),
        (0, 130)
    );
    let mut c = cfg();
    let half = Weights {
        rx_to_in_ppm: 500_000,
        rx_to_out_ppm: 500_000,
        tx_to_in_ppm: 500_000,
        tx_to_out_ppm: 500_000,
    };
    c.weights.insert("P".into(), half);
    let mut state = State::seed(&c, &snapshot((0, 0), (0, 0)), Totals::default()).unwrap();
    state.advance(&c, &snapshot((10, 20), (0, 0))).unwrap();
    assert_eq!(
        (state.totals.billing_in, state.totals.billing_out),
        (15, 15)
    );
}

#[test]
fn segmentation_restart_and_fractional_remainders_are_equivalent() {
    let mut c = cfg();
    c.weights.insert(
        "P".into(),
        Weights {
            rx_to_in_ppm: 500_000,
            tx_to_out_ppm: 333_333,
            ..Weights::default()
        },
    );
    let start = State::seed(&c, &snapshot((0, 0), (0, 0)), Totals::default()).unwrap();
    let mut one = start.clone();
    one.advance(&c, &snapshot((101, 101), (10, 20))).unwrap();
    let mut many = start;
    for n in 1..=101 {
        many.advance(&c, &snapshot((n, n), (10 * n / 101, 20 * n / 101)))
            .unwrap();
        many = serde_json::from_slice(&serde_json::to_vec(&many).unwrap()).unwrap();
    }
    assert_eq!(one.totals, many.totals);
    assert_eq!(many.totals.remainder_in_ppm, 500_000);
    assert_eq!(many.totals.remainder_out_ppm, 666_633);
}

#[test]
fn one_direction_reset_keeps_other_direction() {
    let c = cfg();
    let mut state = State::seed(&c, &snapshot((100, 200), (0, 0)), Totals::default()).unwrap();
    assert_eq!(
        state.advance(&c, &snapshot((0, 220), (0, 0))).unwrap(),
        (0, 20)
    );
    assert_eq!(
        state.advance(&c, &snapshot((10, 230), (0, 0))).unwrap(),
        (10, 10)
    );
}

#[test]
fn identity_boot_and_name_change_never_charge_historical_counters() {
    let mut c = cfg();
    c.strict = false;
    let mut state = State::seed(&c, &snapshot((100, 200), (0, 0)), Totals::default()).unwrap();
    let mut changed = snapshot((100_000, 200_000), (0, 0));
    changed.interfaces.get_mut("P").unwrap().identity = "replacement".into();
    assert_eq!(state.advance(&c, &changed).unwrap(), (0, 0));
    changed.boot = "boot-2".into();
    assert_eq!(state.advance(&c, &changed).unwrap(), (0, 0));
    changed.interfaces.remove("P");
    state.advance(&c, &changed).unwrap();
    changed.interfaces.insert(
        "P".into(),
        snapshot((999_999, 999_999), (0, 0))
            .interfaces
            .remove("P")
            .unwrap(),
    );
    assert_eq!(state.advance(&c, &changed).unwrap(), (0, 0));
    let nic = changed.interfaces.remove("P").unwrap();
    changed.interfaces.insert("renamed".into(), nic);
    assert_eq!(state.advance(&c, &changed).unwrap(), (0, 0));
    assert_eq!(state.totals.raw_in, 0);
}

#[test]
fn policy_change_only_applies_to_future_intervals() {
    let mut c = cfg();
    let mut state = State::seed(&c, &snapshot((0, 0), (0, 0)), Totals::default()).unwrap();
    state.advance(&c, &snapshot((10, 10), (0, 0))).unwrap();
    c.weights.insert(
        "P".into(),
        Weights {
            rx_to_in_ppm: 2_000_000,
            ..Weights::default()
        },
    );
    state.advance(&c, &snapshot((1000, 1000), (0, 0))).unwrap();
    assert_eq!(state.totals.billing_in, 10);
    state.advance(&c, &snapshot((1010, 1010), (0, 0))).unwrap();
    assert_eq!(state.totals.billing_in, 30);
    c.report = ReportMode::Billing;
    assert!(state.advance(&c, &snapshot((1020, 1020), (0, 0))).is_err());
}

#[test]
fn exclusion_wins_and_strict_missing_rejects_the_sample() {
    let mut c = cfg();
    c.exclude = vec!["P".into()];
    let mut snap = snapshot((0, 0), (0, 0));
    snap.interfaces.remove("P");
    let mut state = State::seed(&c, &snap, Totals::default()).unwrap();
    let before = serde_json::to_string(&state).unwrap();
    let mut empty = snap.clone();
    empty.interfaces.clear();
    assert!(state.advance(&c, &empty).is_err());
    assert_eq!(serde_json::to_string(&state).unwrap(), before);
    assert!(!state.previous.contains_key("P"));
}

#[test]
fn overflow_rejects_transaction_without_partial_mutation() {
    let c = japanese();
    let mut state = State::seed(
        &c,
        &snapshot((0, 0), (0, 0)),
        Totals {
            billing_out: LIMIT - 1,
            ..Totals::default()
        },
    )
    .unwrap();
    let before = serde_json::to_string(&state).unwrap();
    assert!(state.advance(&c, &snapshot((1, 1), (1, 1))).is_err());
    assert_eq!(serde_json::to_string(&state).unwrap(), before);
    assert!(
        Totals {
            raw_in: LIMIT + 1,
            ..Totals::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Totals {
            remainder_in_ppm: 1_000_000,
            ..Totals::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn reject_negative_nan_float_excessive_and_unknown_weights() {
    for value in ["-1", "nan", "0.5", "1000000001", "18446744073709551616"] {
        let result = parse_policy(&format!("[weights.P]\nrx_to_in_ppm = {value}"));
        assert!(result.is_err(), "{value}");
    }
    assert!(parse_policy("[weights.P]\nrx_to_out = 2").is_err());
}
fn parse_policy(toml: &str) -> Result<TrafficConfig> {
    use figment::{
        Figment,
        providers::{Format, Toml},
    };
    let cfg: TrafficConfig = Figment::new().merge(Toml::string(toml)).extract()?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(unix)]
#[test]
fn private_atomic_store_lock_checksum_and_restart() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("traffic");
    let path = dir.to_str().unwrap();
    let store = Store::lock(path).unwrap();
    assert!(Store::lock(path).is_err());
    let c = cfg();
    let mut state = State::seed(
        &c,
        &snapshot((0, 0), (0, 0)),
        Totals {
            raw_in: 123,
            billing_in: 123,
            ..Totals::default()
        },
    )
    .unwrap();
    state.advance(&c, &snapshot((10, 10), (0, 0))).unwrap();
    checkpoint(&store, &mut state).unwrap();
    let read = Store::read(path).unwrap();
    assert_eq!(read.totals.raw_in, 133);
    assert_eq!(
        std::fs::metadata(dir.join("traffic-state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(store);
    let _lock = Store::lock(path).unwrap();
    let mut restart = Store::read(path).unwrap();
    restart.advance(&c, &snapshot((15, 15), (0, 0))).unwrap();
    assert_eq!(restart.totals.raw_in, 138);
    let file = dir.join("traffic-state.json");
    let contents = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, contents.replace("133", "134")).unwrap();
    assert!(Store::read(path).is_err());
    std::fs::write(&file, "{").unwrap();
    assert!(Store::read(path).is_err());
}

#[cfg(unix)]
#[test]
fn reject_permissive_paths_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let bad = temp.path().join("bad");
    std::fs::create_dir(&bad).unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::lock(bad.to_str().unwrap()).is_err());
    let link = temp.path().join("link");
    symlink(&bad, &link).unwrap();
    assert!(Store::lock(link.to_str().unwrap()).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn real_linux_snapshot_has_stable_identity_and_byte_counters() {
    let a = linux::snapshot(&TrafficConfig {
        include: vec!["lo".into()],
        ..TrafficConfig::default()
    })
    .unwrap();
    assert!(!a.boot.is_empty());
    assert!(a.interfaces.contains_key("lo"));
    assert!(a.interfaces.values().all(|i| !i.identity.is_empty()));
}

#[cfg(unix)]
fn seeded_meter(cfg: TrafficConfig, store: Store, start: Instant) -> Meter {
    let mut state = State::seed(
        &cfg,
        &snapshot((100, 200), (300, 400)),
        Totals {
            raw_in: 10,
            raw_out: 20,
            billing_in: 100,
            billing_out: 200,
            ..Totals::default()
        },
    )
    .unwrap();
    checkpoint(&store, &mut state).unwrap();
    Meter {
        published_totals: state.totals.reported(cfg.report),
        cfg,
        store,
        state,
        last_sample: start,
        last_flush: Some(start),
    }
}

#[cfg(unix)]
#[test]
fn native_report_combines_unweighted_rates_and_durable_selected_totals() {
    use serverbee_common::protocol::AgentMessage;

    let temp = tempfile::tempdir().unwrap();
    let mut cfg = japanese();
    cfg.report = ReportMode::Billing;
    cfg.flush_interval_secs = 6;
    let dir = temp.path().join("traffic");
    let dir = dir.to_str().unwrap();
    let start = Instant::now() - Duration::from_secs(6);
    let mut meter = seeded_meter(cfg, Store::lock(dir).unwrap(), start);
    let mut report = SystemReport {
        cpu: 42.0,
        ..SystemReport::default()
    };
    let reading = meter
        .sample(
            Ok(snapshot((109, 218), (303, 412))),
            start + Duration::from_secs(3),
        )
        .unwrap();
    reading.apply(&mut report).unwrap();
    assert_eq!(
        (report.net_in_transfer, report.net_out_transfer),
        (100, 200)
    );
    assert_eq!((report.net_in_speed, report.net_out_speed), (4, 10));
    assert_eq!(Store::read(dir).unwrap().totals.billing_out, 200);

    let reading = meter
        .sample(
            Ok(snapshot((112, 230), (309, 430))),
            start + Duration::from_secs(6),
        )
        .unwrap();
    reading.apply(&mut report).unwrap();
    assert_eq!(
        (report.net_in_transfer, report.net_out_transfer),
        (100, 299)
    );
    assert_eq!((report.net_in_speed, report.net_out_speed), (3, 10));
    assert_eq!(Store::read(dir).unwrap().totals.billing_out, 299);
    assert_eq!(meter.state.totals.reported(ReportMode::Raw), (31, 80));
    assert_eq!(report.cpu, 42.0);

    let wire = serde_json::to_value(AgentMessage::Report(report)).unwrap();
    let AgentMessage::Report(decoded) = serde_json::from_value(wire).unwrap() else {
        panic!("expected native Agent report");
    };
    assert_eq!(decoded.net_out_transfer, 299);
    assert_eq!(decoded.net_out_speed, 10);

    let (tx, rx) = watch::channel(Ok(reading));
    let monitor = Monitor(rx);
    tx.send(Err("sample unavailable".into())).unwrap();
    assert!(monitor.apply(&mut SystemReport::default()).is_err());
    tx.send(Ok(Reading {
        sampled_at: Instant::now() - MAX_SAMPLE_AGE,
        totals: (100, 299),
        speed: (3, 10),
    }))
    .unwrap();
    assert!(monitor.apply(&mut SystemReport::default()).is_err());
}

#[cfg(unix)]
#[test]
fn strict_missing_commits_baseline_invalidation_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("traffic");
    let dir = dir.to_str().unwrap();
    let start = Instant::now();
    let mut meter = seeded_meter(cfg(), Store::lock(dir).unwrap(), start);
    meter
        .sample(
            Ok(snapshot((109, 218), (303, 412))),
            start + Duration::from_secs(3),
        )
        .unwrap();
    let mut missing = snapshot((109, 218), (303, 412));
    missing.interfaces.remove("P");
    let error = meter
        .sample(Ok(missing), start + Duration::from_secs(6))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required traffic interface missing: P")
    );
    drop(meter);

    let mut durable = Store::read(dir).unwrap();
    assert_eq!(durable.totals.reported(ReportMode::Raw), (22, 50));
    assert!(durable.previous.is_empty());
    assert_eq!(
        durable
            .advance(&cfg(), &snapshot((10_000, 20_000), (30_000, 40_000)))
            .unwrap(),
        (0, 0)
    );
    assert_eq!(
        durable
            .advance(&cfg(), &snapshot((10_001, 20_002), (30_003, 40_004)))
            .unwrap(),
        (4, 6)
    );
    assert_eq!(durable.totals.reported(ReportMode::Raw), (26, 56));
}

#[cfg(unix)]
#[test]
fn checkpoint_failure_withholds_report_and_retry_preserves_observed_increments() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("traffic");
    let path = dir.to_str().unwrap();
    let start = Instant::now();
    let mut cfg = cfg();
    cfg.flush_interval_secs = 5;
    let mut meter = seeded_meter(cfg, Store::lock(path).unwrap(), start);
    let temporary = dir.join("traffic-state.next");
    std::fs::create_dir(&temporary).unwrap();
    assert!(
        meter
            .sample(
                Ok(snapshot((109, 218), (303, 412))),
                start + Duration::from_secs(6)
            )
            .is_err()
    );
    assert_eq!(Store::read(path).unwrap().totals.raw_in, 10);
    std::fs::remove_dir(temporary).unwrap();
    let reading = meter
        .sample(
            Ok(snapshot((112, 230), (309, 430))),
            start + Duration::from_secs(9),
        )
        .unwrap();
    assert_eq!(reading.totals, (31, 80));
    assert_eq!(Store::read(path).unwrap().totals.raw_in, 31);
}
