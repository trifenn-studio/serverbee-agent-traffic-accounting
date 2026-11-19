//! Opt-in interface accounting for the native Agent report.
pub mod cli;
mod config;
mod ledger;
mod linux;
mod store;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serverbee_common::types::SystemReport;
use tokio::sync::watch;

pub use config::TrafficConfig;
use ledger::{Snapshot, State};
use store::Store;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
const MAX_SAMPLE_AGE: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct Reading {
    sampled_at: Instant,
    totals: (i64, i64),
    speed: (i64, i64),
}

impl Reading {
    fn apply(&self, report: &mut SystemReport) -> Result<()> {
        ensure!(
            self.sampled_at.elapsed() < MAX_SAMPLE_AGE,
            "traffic sampler stale"
        );
        (report.net_in_transfer, report.net_out_transfer) = self.totals;
        (report.net_in_speed, report.net_out_speed) = self.speed;
        Ok(())
    }
}

struct Meter {
    cfg: TrafficConfig,
    store: Store,
    state: State,
    published_totals: (i64, i64),
    last_sample: Instant,
    last_flush: Option<Instant>,
}

impl Meter {
    fn flush(&mut self, now: Instant) -> Result<()> {
        checkpoint(&self.store, &mut self.state).context("saving traffic checkpoint")?;
        self.published_totals = self.state.totals.reported(self.cfg.report);
        self.last_flush = Some(now);
        Ok(())
    }

    fn sample(&mut self, snapshot: Result<Snapshot>, now: Instant) -> Result<Reading> {
        let snapshot = match snapshot.and_then(|snapshot| {
            snapshot.require_complete(&self.cfg)?;
            Ok(snapshot)
        }) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // Preserve accrued totals and record loss of the baseline across
                // a restart. Retry the checkpoint if it did not reach disk.
                if !self.state.previous.is_empty() {
                    self.state.previous.clear();
                    self.last_flush = None;
                }
                if self.last_flush.is_none() {
                    self.flush(now)
                        .with_context(|| format!("invalidating baseline after {error:#}"))?;
                }
                return Err(error);
            }
        };
        let (rx, tx) = self.state.advance(&self.cfg, &snapshot)?;
        let seconds = now.duration_since(self.last_sample).as_secs_f64().max(1.0);
        self.last_sample = now;
        if self.last_flush.is_none_or(|last| {
            now.duration_since(last) >= Duration::from_secs(self.cfg.flush_interval_secs)
        }) {
            self.flush(now)?;
        }
        Ok(Reading {
            sampled_at: now,
            totals: self.published_totals,
            speed: ((rx as f64 / seconds) as i64, (tx as f64 / seconds) as i64),
        })
    }
}

pub struct Monitor(watch::Receiver<Result<Reading, String>>);

impl Monitor {
    pub fn start(cfg: &TrafficConfig) -> Result<Option<Self>> {
        if !cfg.enabled {
            return Ok(None);
        }
        cfg.validate()?;
        ensure!(
            cfg!(target_os = "linux"),
            "interface traffic accounting requires Linux procfs and sysfs"
        );
        let store = Store::lock(&cfg.state_dir)?;
        let state = Store::read(&cfg.state_dir)
            .context("traffic state required; use traffic-seed to initialize a new ledger")?;
        state.require_report_mode(cfg)?;
        let snapshot = linux::snapshot(cfg);
        let now = Instant::now();
        let mut meter = Meter {
            cfg: cfg.clone(),
            store,
            published_totals: state.totals.reported(cfg.report),
            state,
            last_sample: now,
            last_flush: None,
        };
        let (tx, rx) = watch::channel(Ok(Reading {
            speed: (0, 0),
            ..meter.sample(snapshot, now)?
        }));
        tokio::spawn(run_sampler(meter, tx));
        Ok(Some(Self(rx)))
    }

    pub fn apply(&self, report: &mut SystemReport) -> Result<()> {
        match &*self.0.borrow() {
            Ok(reading) => reading.apply(report),
            Err(error) => anyhow::bail!("{error}"),
        }
    }
}

async fn run_sampler(mut meter: Meter, tx: watch::Sender<Result<Reading, String>>) {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + SAMPLE_INTERVAL,
        SAMPLE_INTERVAL,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tx.closed() => break,
            _ = interval.tick() => {}
        }
        // The sampler owns the only writer. A slow fsync cannot block the
        // Reporter or prevent it from checking the age of the last sample.
        let (next, reading) = match tokio::task::spawn_blocking(move || {
            let snapshot = linux::snapshot(&meter.cfg);
            let reading = meter.sample(snapshot, Instant::now());
            (meter, reading)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(%error, "Traffic sampler stopped");
                let _ = tx.send(Err(error.to_string()));
                break;
            }
        };
        meter = next;
        if let Err(error) = &reading {
            tracing::error!(error = %format!("{error:#}"), "Traffic accounting failed; reports withheld");
        }
        if tx
            .send(reading.map_err(|error| format!("{error:#}")))
            .is_err()
        {
            break;
        }
    }
}

fn checkpoint(store: &Store, state: &mut State) -> Result<()> {
    state.checkpoint_unix_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    store.write(state)
}

#[cfg(test)]
mod tests;
