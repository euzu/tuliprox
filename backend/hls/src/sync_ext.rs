use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock access for state whose invariants survive a panicking holder.
pub(crate) trait MutexExt<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> { self.lock().unwrap_or_else(PoisonError::into_inner) }
}
