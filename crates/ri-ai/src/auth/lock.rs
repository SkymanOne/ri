//! A cross-process file lock compatible with `proper-lockfile`, which pi uses
//! for `auth.json`: the lock is a `<file>.lock` directory, and one whose
//! modification time is older than the stale period may be taken over.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;

/// pi's `stale` option for `auth.json`.
const STALE: Duration = Duration::from_secs(30);
const MAX_DELAY: Duration = Duration::from_secs(1);

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The operation was cancelled while waiting.
    #[error("Login cancelled")]
    Cancelled,
    /// Another holder kept the lock for the whole stale period.
    #[error("Lock file is already being held")]
    Held,
    /// Creating the lock directory failed for another reason.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct FileLock {
    dir: PathBuf,
}

impl FileLock {
    /// Locks `file`, retrying with jittered backoff until the stale period has
    /// passed.
    pub async fn acquire(file: &Path, cancel: &CancellationToken) -> Result<FileLock, LockError> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let dir = PathBuf::from(name);
        let deadline = Instant::now() + STALE;
        let mut retry = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Err(LockError::Cancelled);
            }
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(FileLock { dir }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_stale(&dir) {
                        // Another process may take it over first; either way, try again.
                        let _ = std::fs::remove_dir(&dir);
                        continue;
                    }
                }
                Err(err) => return Err(err.into()),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LockError::Held);
            }
            let base =
                Duration::from_millis(10u64.saturating_mul(1 << retry.min(10))).min(MAX_DELAY / 2);
            retry += 1;
            let delay = (base + base.mul_f64(jitter())).min(remaining);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = cancel.cancelled() => return Err(LockError::Cancelled),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn is_stale(dir: &Path) -> bool {
    std::fs::metadata(dir)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE)
}

/// A value in `[0, 1)`; spreads retries of competing processes.
fn jitter() -> f64 {
    let mut bytes = [0u8; 2];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u16::from_le_bytes(bytes)) / 65536.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn excludes_and_releases() {
        let dir = std::env::temp_dir().join(format!("ri-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        let cancel = CancellationToken::new();
        let held = FileLock::acquire(&file, &cancel).await.unwrap();
        assert!(dir.join("auth.json.lock").is_dir());
        let waiting = tokio::spawn({
            let file = file.clone();
            let cancel = cancel.clone();
            async move { FileLock::acquire(&file, &cancel).await.map(drop) }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        drop(held);
        waiting.await.unwrap().unwrap();
        assert!(!dir.join("auth.json.lock").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
