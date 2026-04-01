use core::sync::atomic::{AtomicBool, Ordering};

/// RAII guard that releases the spin lock on drop, ensuring panic safety.
///
/// If a panic occurs while the lock is held (e.g. from a `debug_assert!`),
/// the guard's `Drop` impl will release the lock, preventing permanent
/// deadlock of all threads.
pub(crate) struct LockGuard<'a> {
    lock: &'a AtomicBool,
}

impl<'a> LockGuard<'a> {
    pub(crate) fn acquire(lock: &'a AtomicBool) -> Self {
        while lock.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Self { lock }
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}
