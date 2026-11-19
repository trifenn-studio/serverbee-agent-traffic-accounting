use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ledger::State;

const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    state: State,
    sha256: String,
}

pub struct Store {
    dir: PathBuf,
    _lock: File,
}

fn check_private(path: &Path, directory: bool) -> Result<()> {
    check_private_metadata(path, &fs::symlink_metadata(path)?, directory)
}

#[cfg(unix)]
fn check_private_metadata(path: &Path, meta: &fs::Metadata, directory: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        },
        "traffic path has wrong type or is a symlink: {}",
        path.display()
    );
    let mode = if directory { 0o700 } else { 0o600 };
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.permissions().mode() & 0o777 == mode,
        "traffic path must be owned by current user with mode {mode:o}: {}",
        path.display(),
    );
    Ok(())
}

#[cfg(not(unix))]
fn check_private_metadata(_: &Path, _: &fs::Metadata, _: bool) -> Result<()> {
    anyhow::bail!("persistent traffic metering requires Linux/Unix private filesystem semantics")
}

fn open_private(path: &Path, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    check_private_metadata(path, &file.metadata()?, false)?;
    Ok(file)
}

impl Store {
    pub fn lock(dir: &str) -> Result<Self> {
        let dir = PathBuf::from(dir);
        ensure!(
            dir.is_absolute(),
            "traffic state directory must be absolute"
        );
        if !dir.try_exists()? {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&dir)?;
        }
        check_private(&dir, true)?;
        let path = dir.join("traffic.lock");
        let lock = match open_private(&path, true) {
            Ok(lock) => lock,
            Err(error) if path.try_exists()? => open_private(&path, false)
                .with_context(|| format!("opening traffic lock after: {error}"))?,
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            ensure!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "traffic ledger already locked by another instance: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { dir, _lock: lock })
    }

    pub fn read(dir: &str) -> Result<State> {
        let dir = Path::new(dir);
        check_private(dir, true)?;
        let path = dir.join("traffic-state.json");
        check_private(&path, false)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        check_private_metadata(&path, &metadata, false)?;
        ensure!(metadata.len() <= MAX_STATE_BYTES, "traffic state too large");
        let mut data = Vec::new();
        file.take(MAX_STATE_BYTES + 1).read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 <= MAX_STATE_BYTES,
            "traffic state too large"
        );
        let envelope: Envelope = serde_json::from_slice(&data).context(
            "corrupt traffic state JSON; restore a verified checkpoint, never silently reset",
        )?;
        ensure!(
            envelope.sha256 == hex::encode(Sha256::digest(serde_json::to_vec(&envelope.state)?)),
            "traffic state checksum mismatch"
        );
        envelope.state.validate()?;
        Ok(envelope.state)
    }

    pub fn exists(&self) -> Result<bool> {
        // symlink_metadata distinguishes absent state from a dangling symlink.
        match fs::symlink_metadata(self.dir.join("traffic-state.json")) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write(&self, state: &State) -> Result<()> {
        state.validate()?;
        let envelope = Envelope {
            sha256: hex::encode(Sha256::digest(serde_json::to_vec(state)?)),
            state: state.clone(),
        };
        let data = serde_json::to_vec(&envelope)?;
        ensure!(
            data.len() as u64 <= MAX_STATE_BYTES,
            "traffic state too large"
        );
        let temporary = self.dir.join("traffic-state.next");
        // A leftover interrupted write is never used as a checkpoint.
        if fs::symlink_metadata(&temporary).is_ok() {
            check_private(&temporary, false)?;
            fs::remove_file(&temporary)?;
        }
        let mut file = open_private(&temporary, true)?;
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&temporary, self.dir.join("traffic-state.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
