//! Serializes writes to the same file across concurrently running tool calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Runs `work` once every earlier mutation of the same file (by real path) has
/// finished.
pub async fn with_file_lock<T>(path: &Path, work: impl Future<Output = T>) -> T {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let lock = {
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(key).or_default().clone()
    };
    let _guard = lock.lock().await;
    work.await
}
