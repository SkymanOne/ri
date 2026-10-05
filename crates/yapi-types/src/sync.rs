//! Locking that outlives a panic.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, taking the data even when a panicking thread poisoned it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
