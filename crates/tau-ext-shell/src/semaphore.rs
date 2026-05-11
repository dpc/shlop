//! Counting semaphore with owned permits.
//!
//! Permits are owned so they can be moved across thread boundaries —
//! the dispatcher loop acquires a permit *before* spawning the worker
//! that holds it, which bounds the in-flight thread count rather than
//! just the concurrent-execution count.

use std::sync::{Arc, Condvar, Mutex};

pub(crate) struct Semaphore {
    state: Mutex<usize>,
    cond: Condvar,
}

/// Owned permit; releases on drop.
pub(crate) struct OwnedPermit(Arc<Semaphore>);

impl Semaphore {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(permits),
            cond: Condvar::new(),
        }
    }

    /// Block until a permit is available, then take it.
    pub(crate) fn acquire(self: &Arc<Self>) -> OwnedPermit {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count == 0 {
            count = self.cond.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        OwnedPermit(Arc::clone(self))
    }
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        let mut count = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
        self.0.cond.notify_one();
    }
}
