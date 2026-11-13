//! The data directory: shards, locks, and the folder default excludes.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::paths;

/// Default excludes of the folder source, at the lowest rule precedence.
pub(crate) const FOLDER_DEFAULTS: &str =
    ".*\nnode_modules/\ndist/\nbuild/\ncoverage/\ntarget/\nvendor/\n";

#[derive(Debug, Clone)]
pub(crate) struct Home {
    dir: PathBuf,
}

impl Home {
    pub fn open(dir: &Path) -> Result<Self> {
        for sub in ["roots", "locks"] {
            let path = dir.join(sub);
            fs::create_dir_all(&path).map_err(|error| Error::io(&path, &error))?;
        }
        let dir = paths::canonical(dir).map_err(|error| Error::io(dir, &error))?;
        let home = Self { dir };
        let defaults = home.defaults_path();
        if fs::read_to_string(&defaults).ok().as_deref() != Some(FOLDER_DEFAULTS) {
            // A rename, so a concurrent walk never reads a partly written file.
            let temp = defaults.with_extension(format!("{}.tmp", std::process::id()));
            fs::write(&temp, FOLDER_DEFAULTS).map_err(|error| Error::io(&temp, &error))?;
            fs::rename(&temp, &defaults).map_err(|error| Error::io(&defaults, &error))?;
        }
        Ok(home)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn defaults_path(&self) -> PathBuf {
        self.dir.join("folder-defaults.ignore")
    }

    pub fn roots_dir(&self) -> PathBuf {
        self.dir.join("roots")
    }

    pub fn shard_path(&self, root_key: &str) -> PathBuf {
        self.roots_dir()
            .join(format!("{}.sqlite", paths::key_hash(root_key)))
    }

    /// The key hash in the name of a shard file or its WAL, shared-memory, or staging file.
    pub fn shard_hash(file_name: &str) -> &str {
        file_name.split('.').next().unwrap_or_default()
    }

    /// Takes the advisory refresh lock of one root. `None` waits without a limit.
    pub fn lock(&self, root_key: &str, timeout: Option<Duration>) -> Result<RootLock> {
        self.lock_hash(&paths::key_hash(root_key), timeout)
    }

    /// Like [`Self::lock`], for a root known only by its key hash (a shard file stem).
    pub fn lock_hash(&self, hash: &str, timeout: Option<Duration>) -> Result<RootLock> {
        let path = self.dir.join("locks").join(format!("{hash}.lock"));
        let file = File::create(&path).map_err(|error| Error::io(&path, &error))?;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(RootLock { _file: file }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(Error::io(&path, &error)),
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::temporary(
                    "timed out waiting for another refresh of this root",
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Holds a root's refresh lock until dropped; closing the file releases it.
#[derive(Debug)]
pub(crate) struct RootLock {
    _file: File,
}

/// `%LOCALAPPDATA%\lantern`, `~/Library/Application Support/lantern`, or the XDG data directory.
#[must_use]
pub fn default_home() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        std::env::home_dir()?.join("Library/Application Support")
    } else {
        match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()?.join(".local/share"),
        }
    };
    Some(base.join("lantern"))
}
