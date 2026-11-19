use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::config::{PPM, ReportMode, TrafficConfig};

pub const LIMIT: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    pub raw_in: u64,
    pub raw_out: u64,
    pub billing_in: u64,
    pub billing_out: u64,
    pub remainder_in_ppm: u64,
    pub remainder_out_ppm: u64,
}

impl Totals {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [self.raw_in, self.raw_out, self.billing_in, self.billing_out]
                .into_iter()
                .all(|n| n <= LIMIT),
            "traffic total exceeds native signed byte field"
        );
        ensure!(
            self.remainder_in_ppm < PPM && self.remainder_out_ppm < PPM,
            "invalid traffic fractional remainder"
        );
        Ok(())
    }

    fn add(&mut self, rx: u64, tx: u64, cfg: &super::config::Weights) -> Result<()> {
        self.raw_in = add(self.raw_in, rx)?;
        self.raw_out = add(self.raw_out, tx)?;
        let incoming = rx as u128 * cfg.rx_to_in_ppm as u128
            + tx as u128 * cfg.tx_to_in_ppm as u128
            + self.remainder_in_ppm as u128;
        let outgoing = rx as u128 * cfg.rx_to_out_ppm as u128
            + tx as u128 * cfg.tx_to_out_ppm as u128
            + self.remainder_out_ppm as u128;
        self.billing_in = add(self.billing_in, u64::try_from(incoming / PPM as u128)?)?;
        self.billing_out = add(self.billing_out, u64::try_from(outgoing / PPM as u128)?)?;
        self.remainder_in_ppm = (incoming % PPM as u128) as u64;
        self.remainder_out_ppm = (outgoing % PPM as u128) as u64;
        Ok(())
    }

    pub fn reported(&self, mode: ReportMode) -> (i64, i64) {
        match mode {
            ReportMode::Raw => (self.raw_in as i64, self.raw_out as i64),
            ReportMode::Billing => (self.billing_in as i64, self.billing_out as i64),
        }
    }
}

fn add(a: u64, b: u64) -> Result<u64> {
    let n = a
        .checked_add(b)
        .ok_or_else(|| anyhow::anyhow!("traffic byte overflow"))?;
    ensure!(n <= LIMIT, "traffic byte overflow: native field is i64");
    Ok(n)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub identity: String,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub boot: String,
    pub interfaces: BTreeMap<String, Interface>,
}

impl Snapshot {
    pub fn missing<'a>(&self, cfg: &'a TrafficConfig) -> impl Iterator<Item = &'a str> {
        cfg.include
            .iter()
            .map(String::as_str)
            .filter(|name| cfg.selected(name) && !self.interfaces.contains_key(*name))
    }

    pub fn require_complete(&self, cfg: &TrafficConfig) -> Result<()> {
        if cfg.strict
            && let Some(name) = self.missing(cfg).next()
        {
            anyhow::bail!("required traffic interface missing: {name}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema_version: u32,
    pub checkpoint_unix_secs: u64,
    pub boot: String,
    pub policy_hash: String,
    pub report: ReportMode,
    pub totals: Totals,
    /// Per-name audit since seed, including retired names. Seed offsets are global only.
    pub interface_totals: BTreeMap<String, Totals>,
    pub previous: BTreeMap<String, Interface>,
}

impl State {
    pub fn require_report_mode(&self, cfg: &TrafficConfig) -> Result<()> {
        ensure!(
            self.report == cfg.report,
            "traffic report mode changed; explicit offline reseed is required to preserve reported cumulative values"
        );
        Ok(())
    }

    pub fn seed(cfg: &TrafficConfig, snapshot: &Snapshot, totals: Totals) -> Result<Self> {
        cfg.validate()?;
        totals.validate()?;
        let mut state = Self {
            schema_version: 1,
            checkpoint_unix_secs: 0,
            boot: String::new(),
            policy_hash: String::new(),
            report: cfg.report,
            totals,
            interface_totals: BTreeMap::new(),
            previous: BTreeMap::new(),
        };
        state.advance(cfg, snapshot)?;
        Ok(state)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported traffic state schema");
        ensure!(
            !self.boot.is_empty() && self.policy_hash.len() == 64,
            "invalid traffic baseline metadata"
        );
        ensure!(
            self.previous.len() <= 4096 && self.interface_totals.len() <= 4096,
            "traffic interface history limit exceeded"
        );
        ensure!(
            self.previous.values().all(|i| !i.identity.is_empty()),
            "invalid traffic interface identity"
        );
        self.totals.validate()?;
        for totals in self.interface_totals.values() {
            totals.validate()?;
        }
        Ok(())
    }

    /// Transactional: rejected samples never change counters, remainders or baselines.
    pub fn advance(&mut self, cfg: &TrafficConfig, snapshot: &Snapshot) -> Result<(u64, u64)> {
        self.require_report_mode(cfg)?;
        ensure!(!snapshot.boot.is_empty(), "empty kernel boot identity");
        snapshot.require_complete(cfg)?;
        let policy_hash = cfg.policy_hash()?;
        let reset = self.boot != snapshot.boot || self.policy_hash != policy_hash;
        let mut next = self.clone();
        next.previous.clear();
        let (mut delta_rx, mut delta_tx) = (0, 0);
        for (name, interface) in &snapshot.interfaces {
            if !cfg.selected(name) {
                continue;
            }
            ensure!(
                !interface.identity.is_empty(),
                "empty interface identity: {name}"
            );
            let (rx, tx) = match self.previous.get(name) {
                Some(old) if !reset && old.identity == interface.identity => (
                    interface.rx.saturating_sub(old.rx),
                    interface.tx.saturating_sub(old.tx),
                ),
                _ => (0, 0),
            };
            delta_rx = add(delta_rx, rx)?;
            delta_tx = add(delta_tx, tx)?;
            let weight = cfg.weights.get(name).cloned().unwrap_or_default();
            next.totals.add(rx, tx, &weight)?;
            next.interface_totals
                .entry(name.clone())
                .or_default()
                .add(rx, tx, &weight)?;
            next.previous.insert(name.clone(), interface.clone());
        }
        next.boot.clone_from(&snapshot.boot);
        next.policy_hash = policy_hash;
        next.validate()?;
        *self = next;
        Ok((delta_rx, delta_tx))
    }
}
