use anyhow::Result;

use super::{TrafficConfig, ledger::Snapshot};

#[cfg(target_os = "linux")]
pub fn snapshot(cfg: &TrafficConfig) -> Result<Snapshot> {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    let boot = format!(
        "{}:{}",
        fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
        fs::metadata("/proc/self/ns/net")?.ino()
    );
    let sysfs = Path::new("/sys/class/net");
    let names = fs::read_dir(sysfs)?
        .map(|entry| entry.map(|entry| entry.file_name().as_bytes().to_vec()))
        .collect::<std::io::Result<Vec<_>>>()?;
    snapshot_with(
        cfg,
        boot,
        names,
        |name| {
            let dir = sysfs.join(name);
            Ok(format!(
                "{}:{}:{}:{}",
                fs::read_to_string(dir.join("ifindex"))?.trim(),
                fs::read_to_string(dir.join("iflink"))?.trim(),
                fs::read_to_string(dir.join("type"))?.trim(),
                fs::read_to_string(dir.join("address"))?.trim()
            ))
        },
        || Ok(fs::read("/proc/net/dev")?),
    )
}

// Match the selected subset across sysfs and procfs. Other interfaces may
// appear, disappear or be unreadable without affecting that subset.
#[cfg(any(target_os = "linux", test))]
fn snapshot_with(
    cfg: &TrafficConfig,
    boot: String,
    names: impl IntoIterator<Item = Vec<u8>>,
    mut identity: impl FnMut(&str) -> Result<String>,
    read_counters: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Snapshot> {
    use std::collections::BTreeMap;

    use anyhow::{Context, ensure};

    use super::ledger::Interface;

    let mut identities = BTreeMap::new();
    for raw_name in names {
        if let Some(name) = selected_name(cfg, &raw_name)? {
            identities.insert(name.to_owned(), identity(name)?);
        }
    }

    let content = read_counters()?;
    let mut lines = content
        .strip_suffix(b"\n")
        .unwrap_or(&content)
        .split(|&byte| byte == b'\n');
    ensure!(lines.nth(1).is_some(), "invalid /proc/net/dev header");
    let mut interfaces = BTreeMap::new();
    for line in lines {
        let delimiter = line
            .iter()
            .position(|&byte| byte == b':')
            .context("invalid /proc/net/dev row")?;
        let Some(name) = selected_name(cfg, line[..delimiter].trim_ascii())? else {
            continue;
        };
        let before = identities
            .get(name)
            .with_context(|| format!("selected interface {name} appeared during sample"))?;
        let mut counters = str::from_utf8(&line[delimiter + 1..])?.split_ascii_whitespace();
        let mut values = [0u64; 16];
        for value in &mut values {
            *value = counters
                .next()
                .context("missing kernel interface counter")?
                .parse()?;
        }
        ensure!(counters.next().is_none(), "extra kernel interface counters");
        ensure!(
            *before == identity(name)?,
            "selected interface {name} changed identity during sample"
        );
        ensure!(
            interfaces
                .insert(
                    name.to_owned(),
                    Interface {
                        identity: before.clone(),
                        rx: values[0],
                        tx: values[8],
                    }
                )
                .is_none(),
            "duplicate selected kernel interface {name}"
        );
    }
    ensure!(
        interfaces.len() == identities.len(),
        "selected interface disappeared during sample"
    );
    ensure!(
        !cfg.include.is_empty() || !interfaces.is_empty(),
        "no selected kernel interfaces readable"
    );
    Ok(Snapshot { boot, interfaces })
}

#[cfg(any(target_os = "linux", test))]
fn selected_name<'a>(cfg: &TrafficConfig, bytes: &'a [u8]) -> Result<Option<&'a str>> {
    match str::from_utf8(bytes) {
        Ok(name) => Ok(cfg.selected(name).then_some(name)),
        Err(_) if !cfg.include.is_empty() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn snapshot(_: &TrafficConfig) -> Result<Snapshot> {
    anyhow::bail!("interface traffic accounting requires Linux procfs and sysfs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traffic::ledger::{State, Totals};

    fn counters(rx: u64, tx: u64) -> Vec<u8> {
        format!(
            "header\nheader\n ens160: {rx} 0 0 0 0 0 0 0 {tx} 0 0 0 0 0 0 0\nveth-new: 999 0 0 0 0 0 0 0 999 0 0 0 0 0 0 0\n"
        ).into_bytes()
    }

    #[test]
    fn unselected_veth_churn_preserves_selected_interface_deltas() {
        let cfg = TrafficConfig {
            include: vec!["ens160".into()],
            ..TrafficConfig::default()
        };
        let read = |rx, tx| {
            snapshot_with(
                &cfg,
                "boot".into(),
                [b"ens160".to_vec(), b"veth-old".to_vec()],
                |name| {
                    assert_eq!(name, "ens160");
                    Ok("stable-ens160".into())
                },
                || {
                    let mut content = counters(rx, tx);
                    content.extend_from_slice(b"veth-\xff: unread counters\n");
                    Ok(content)
                },
            )
            .unwrap()
        };
        let mut state = State::seed(&cfg, &read(100, 200), Totals::default()).unwrap();
        let next = read(130, 260);
        assert_eq!(next.interfaces.len(), 1);
        assert_eq!(state.advance(&cfg, &next).unwrap(), (30, 60));
        assert_eq!((state.totals.raw_in, state.totals.raw_out), (30, 60));
    }

    #[test]
    fn excluded_veth_churn_is_ignored_with_empty_include() {
        let cfg = TrafficConfig {
            exclude: vec!["veth-old".into(), "veth-new".into()],
            ..TrafficConfig::default()
        };
        let snap = snapshot_with(
            &cfg,
            "boot".into(),
            [b"ens160".to_vec(), b"veth-old".to_vec()],
            |name| {
                assert_eq!(name, "ens160");
                Ok("stable".into())
            },
            || Ok(counters(10, 20)),
        )
        .unwrap();
        assert_eq!(snap.interfaces.len(), 1);
    }

    #[test]
    fn selected_interface_appearance_disappearance_and_identity_change_are_errors() {
        let cfg = TrafficConfig {
            include: vec!["ens160".into()],
            ..TrafficConfig::default()
        };
        let appeared = snapshot_with(
            &cfg,
            "boot".into(),
            [],
            |_| Ok("id".into()),
            || Ok(counters(10, 20)),
        );
        assert!(appeared.unwrap_err().to_string().contains("appeared"));
        let disappeared = snapshot_with(
            &cfg,
            "boot".into(),
            [b"ens160".to_vec()],
            |_| Ok("id".into()),
            || Ok(b"header\nheader\n".to_vec()),
        );
        assert!(disappeared.unwrap_err().to_string().contains("disappeared"));
        let mut reads = 0;
        let changed = snapshot_with(
            &cfg,
            "boot".into(),
            [b"ens160".to_vec()],
            |_| {
                reads += 1;
                Ok(format!("id-{reads}"))
            },
            || Ok(counters(10, 20)),
        );
        assert!(
            changed
                .unwrap_err()
                .to_string()
                .contains("changed identity")
        );
    }
}
