use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PPM: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportMode {
    #[default]
    Raw,
    Billing,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub rx_to_in_ppm: u64,
    pub rx_to_out_ppm: u64,
    pub tx_to_in_ppm: u64,
    pub tx_to_out_ppm: u64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            rx_to_in_ppm: PPM,
            rx_to_out_ppm: 0,
            tx_to_in_ppm: 0,
            tx_to_out_ppm: PPM,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrafficConfig {
    pub enabled: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub strict: bool,
    pub report: ReportMode,
    pub state_dir: String,
    pub flush_interval_secs: u64,
    pub weights: BTreeMap<String, Weights>,
}

impl Default for TrafficConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            include: Vec::new(),
            exclude: Vec::new(),
            strict: true,
            report: ReportMode::Raw,
            state_dir: "/var/lib/serverbee/traffic".to_string(),
            flush_interval_secs: 30,
            weights: BTreeMap::new(),
        }
    }
}

impl TrafficConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (5..=3600).contains(&self.flush_interval_secs),
            "traffic flush_interval_secs must be 5..3600"
        );
        ensure!(
            std::path::Path::new(&self.state_dir).is_absolute(),
            "traffic state_dir must be absolute"
        );
        for name in self
            .include
            .iter()
            .chain(&self.exclude)
            .chain(self.weights.keys())
        {
            ensure!(
                !name.is_empty()
                    && name.len() <= 255
                    && !name.contains(['/', '\0', ':'])
                    && name != "."
                    && name != "..",
                "invalid traffic interface name: {name:?}"
            );
        }
        for names in [&self.include, &self.exclude] {
            ensure!(
                names.iter().collect::<BTreeSet<_>>().len() == names.len(),
                "duplicate traffic interface name"
            );
        }
        for weight in self.weights.values() {
            for value in [
                weight.rx_to_in_ppm,
                weight.rx_to_out_ppm,
                weight.tx_to_in_ppm,
                weight.tx_to_out_ppm,
            ] {
                ensure!(
                    value <= 1_000_000_000,
                    "traffic weight exceeds 1000x (1000000000 ppm)"
                );
            }
        }
        Ok(())
    }

    pub fn selected(&self, name: &str) -> bool {
        !self.exclude.iter().any(|n| n == name)
            && (self.include.is_empty() || self.include.iter().any(|n| n == name))
    }

    pub fn policy_hash(&self) -> Result<String> {
        // Report mode is pinned separately; persistence cadence/path do not alter metering.
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            &self.include,
            &self.exclude,
            self.strict,
            &self.weights,
        ))?)))
    }
}
