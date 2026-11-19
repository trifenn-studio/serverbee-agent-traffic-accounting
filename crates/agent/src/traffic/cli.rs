use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use figment::providers::Serialized;
use serde_json::{Value, json};

use super::{
    TrafficConfig, checkpoint,
    ledger::{State, Totals},
    linux,
    store::Store,
};
use crate::config::agent_figment;

const SCHEMA_VERSION: u32 = 1;

/// Return true after handling an offline command, before Agent startup.
pub fn run(argv: &[String]) -> Result<bool> {
    let Some(command) = argv.get(1).map(String::as_str) else {
        return Ok(false);
    };
    if !matches!(
        command,
        "traffic-check" | "traffic-status" | "traffic-seed" | "traffic-version"
    ) {
        return Ok(false);
    }
    if command == "traffic-version" {
        ensure!(argv.len() == 2, "usage: serverbee-agent traffic-version");
        print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "agent_version": serverbee_common::constants::VERSION,
            "arch": std::env::consts::ARCH,
            "os": std::env::consts::OS,
        }))?;
        return Ok(true);
    }

    let mut args = argv[2..].iter().map(String::as_str);
    let mut flags = BTreeMap::new();
    while let Some(key) = args.next() {
        ensure!(
            key == "--config"
                || (command == "traffic-seed"
                    && matches!(
                        key,
                        "--raw-in" | "--raw-out" | "--billing-in" | "--billing-out"
                    )),
            "unknown traffic option: {key}"
        );
        let value = args
            .next()
            .with_context(|| format!("{key} requires a value"))?;
        ensure!(
            flags.insert(key, value).is_none(),
            "duplicate traffic option: {key}"
        );
    }

    let path = flags.get("--config").map(Path::new);
    if let Some(path) = path {
        ensure!(
            path.is_file(),
            "traffic config file does not exist: {}",
            path.display()
        );
    }
    let cfg: TrafficConfig = agent_figment(path)
        .join(Serialized::defaults(
            json!({ "collector": { "traffic": {} } }),
        ))
        .extract_inner("collector.traffic")?;
    cfg.validate()?;

    match command {
        "traffic-check" => check(&cfg)?,
        "traffic-status" => {
            let state = Store::read(&cfg.state_dir)?;
            print_json(&json!({
                "schema_version": SCHEMA_VERSION,
                "durable_only": true,
                "policy_matches": state.policy_hash == cfg.policy_hash()?,
                "report_matches": state.report == cfg.report,
                "state": state,
            }))?;
        }
        "traffic-seed" => seed(&cfg, &flags)?,
        _ => unreachable!(),
    }
    Ok(true)
}

fn check(cfg: &TrafficConfig) -> Result<()> {
    let snapshot = linux::snapshot(cfg)?;
    let missing: Vec<_> = snapshot.missing(cfg).collect();
    let interfaces: Vec<_> = snapshot
        .interfaces
        .iter()
        .map(|(name, nic)| {
            json!({
                "name": name,
                "identity": nic.identity,
                "rx_bytes": nic.rx,
                "tx_bytes": nic.tx,
                "selected": true,
                "weights": cfg.weights.get(name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    print_json(&json!({
        "schema_version": SCHEMA_VERSION,
        "enabled": cfg.enabled,
        "report": cfg.report,
        "strict": cfg.strict,
        "missing": missing,
        "policy_hash": cfg.policy_hash()?,
        "interfaces": interfaces,
        "state_dir": cfg.state_dir,
        "flush_interval_secs": cfg.flush_interval_secs,
    }))?;
    snapshot.require_complete(cfg)
}

fn seed(cfg: &TrafficConfig, flags: &BTreeMap<&str, &str>) -> Result<()> {
    ensure!(
        cfg.enabled,
        "traffic-seed requires collector.traffic.enabled=true"
    );
    let number = |flag| -> Result<u64> {
        flags
            .get(flag)
            .with_context(|| format!("traffic-seed requires explicit {flag} in bytes"))?
            .parse()
            .with_context(|| format!("{flag} requires a nonnegative integer byte count"))
    };
    let totals = Totals {
        raw_in: number("--raw-in")?,
        raw_out: number("--raw-out")?,
        billing_in: number("--billing-in")?,
        billing_out: number("--billing-out")?,
        ..Totals::default()
    };
    totals.validate()?;
    let mut state = State::seed(cfg, &linux::snapshot(cfg)?, totals)?;
    let store = Store::lock(&cfg.state_dir)?;
    ensure!(
        !store.exists()?,
        "traffic state exists; seed cannot overwrite a ledger"
    );
    checkpoint(&store, &mut state)?;
    print_json(&json!({ "schema_version": SCHEMA_VERSION, "seeded": true, "state": state }))
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
