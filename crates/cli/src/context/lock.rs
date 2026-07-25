//! File locks: a blocking per-remote lock, a non-blocking store/project
//! mutation lock so a second concurrent invocation fails fast instead of
//! silently queueing.

use std::fs::{self, OpenOptions};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub struct FileLock {
    file: fs::File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = open_lock_file(path)?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(Self { file })
    }

    /// Take the lock or fail with `busy`, so a second concurrent invocation
    /// reports the conflict instead of queueing behind it.
    pub fn try_acquire_or_err(path: &Path, busy: impl FnOnce() -> String) -> Result<Self> {
        match Self::try_acquire(path)? {
            Some(lock) => Ok(lock),
            None => bail!(busy()),
        }
    }

    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Best-effort; there is no meaningful recovery path in Drop.
        self.file.unlock().ok();
    }
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))
}
