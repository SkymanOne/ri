//! Locking that outlives a panic.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Locks `mutex`, taking the data even when a panicking thread poisoned it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read-locks `lock`, taking the data even when a panicking thread poisoned it.
pub fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write-locks `lock`, taking the data even when a panicking thread poisoned it.
pub fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_poisoned_rw_locks() {
        let lock = RwLock::new(1);
        let _ = std::panic::catch_unwind(|| {
            let _guard = lock.write();
            panic!("poison");
        });
        assert!(lock.is_poisoned());
        *write(&lock) += 1;
        assert_eq!(*read(&lock), 2);
    }
}
