//! Small filesystem helpers.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// An exclusive `flock` on a file, released on drop (and by the kernel when
/// the process exits or execs: the file is close-on-exec).
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Waits up to `wait` for the lock on `path` (created, with its dir, if
    /// missing). `Ok(None)` when it stays busy.
    pub fn acquire(path: &Path, wait: Duration) -> std::io::Result<Option<FileLock>> {
        use std::os::fd::AsRawFd;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: flock on a descriptor we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(FileLock { _file: file }));
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Write `data` to `path` atomically: temp file in the same dir, then rename.
/// Creates the parent dir.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent dir", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
