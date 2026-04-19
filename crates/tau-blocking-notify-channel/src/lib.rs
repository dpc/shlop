//! A coalescing notify channel with multiple senders and a single receiver.
//!
//! The shared state is conceptually a single `bool`. Senders set it to `true`;
//! the receiver blocks until it becomes `true`, then atomically resets it to
//! `false`. Multiple sends before a receive coalesce into one notification.
//!
//! When every `Sender` has been dropped the channel becomes *disconnected*.
//! A pending notification always takes priority over disconnection: the
//! receiver will see `Ok(())` first and only get `Err(Disconnected)` on the
//! next call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Creates a new notify channel, returning `(Sender, Receiver)`.
pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            notified: false,
            disconnected: false,
        }),
        condvar: Condvar::new(),
        sender_count: AtomicUsize::new(1),
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

/// Error returned by [`Receiver::recv`] and [`Receiver::try_recv`] when all
/// senders have been dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel disconnected")
    }
}

impl std::error::Error for Disconnected {}

struct State {
    notified: bool,
    disconnected: bool,
}

struct Shared {
    state: Mutex<State>,
    condvar: Condvar,
    sender_count: AtomicUsize,
}

/// Sending half of a notify channel. Cloneable for multiple producers.
pub struct Sender {
    shared: Arc<Shared>,
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Sender {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if self.shared.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("notify channel mutex poisoned");
            state.disconnected = true;
            self.shared.condvar.notify_one();
        }
    }
}

impl Sender {
    /// Signal the receiver. If the flag is already set, this is a no-op
    /// (coalescing).
    pub fn notify(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("notify channel mutex poisoned");
        state.notified = true;
        self.shared.condvar.notify_one();
    }
}

/// Receiving half of a notify channel. Not cloneable — single consumer.
pub struct Receiver {
    shared: Arc<Shared>,
}

impl Receiver {
    /// Block until the flag is `true`, then atomically reset it to `false`.
    ///
    /// Returns `Err(Disconnected)` when all senders have been dropped **and**
    /// no pending notification remains.
    pub fn recv(&self) -> Result<(), Disconnected> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("notify channel mutex poisoned");
        loop {
            if state.notified {
                state.notified = false;
                return Ok(());
            }
            if state.disconnected {
                return Err(Disconnected);
            }
            state = self
                .shared
                .condvar
                .wait(state)
                .expect("notify channel mutex poisoned");
        }
    }

    /// Non-blocking check.
    ///
    /// Returns `Ok(true)` if a notification was pending (and resets it),
    /// `Ok(false)` if nothing was pending, or `Err(Disconnected)` when all
    /// senders have been dropped and no notification remains.
    pub fn try_recv(&self) -> Result<bool, Disconnected> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("notify channel mutex poisoned");
        if state.notified {
            state.notified = false;
            return Ok(true);
        }
        if state.disconnected {
            return Err(Disconnected);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn single_notify_wakes_receiver() {
        let (tx, rx) = channel();
        tx.notify();
        assert_eq!(rx.recv(), Ok(()));
    }

    #[test]
    fn multiple_notifies_coalesce() {
        let (tx, rx) = channel();
        tx.notify();
        tx.notify();
        tx.notify();
        assert_eq!(rx.recv(), Ok(()));
        assert_eq!(rx.try_recv(), Ok(false));
    }

    #[test]
    fn try_recv_returns_false_when_not_notified() {
        let (_tx, rx) = channel();
        assert_eq!(rx.try_recv(), Ok(false));
    }

    #[test]
    fn try_recv_returns_true_and_resets() {
        let (tx, rx) = channel();
        tx.notify();
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(rx.try_recv(), Ok(false));
    }

    #[test]
    fn recv_blocks_until_notified() {
        let (tx, rx) = channel();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            tx.notify();
        });
        assert_eq!(rx.recv(), Ok(()));
        handle.join().expect("sender thread panicked");
    }

    #[test]
    fn multiple_senders() {
        let (tx, rx) = channel();
        let tx2 = tx.clone();

        let h1 = thread::spawn(move || {
            tx.notify();
        });
        let h2 = thread::spawn(move || {
            tx2.notify();
        });

        h1.join().expect("sender 1 panicked");
        h2.join().expect("sender 2 panicked");

        assert_eq!(rx.recv(), Ok(()));
        // Both senders are gone — channel is disconnected.
        assert_eq!(rx.try_recv(), Err(Disconnected));
    }

    #[test]
    fn repeated_send_recv_cycles() {
        let (tx, rx) = channel();
        for _ in 0..100 {
            tx.notify();
            assert_eq!(rx.recv(), Ok(()));
            assert_eq!(rx.try_recv(), Ok(false));
        }
    }

    #[test]
    fn disconnect_after_all_senders_dropped() {
        let (tx, rx) = channel();
        drop(tx);
        assert_eq!(rx.recv(), Err(Disconnected));
    }

    #[test]
    fn disconnect_after_last_clone_dropped() {
        let (tx, rx) = channel();
        let tx2 = tx.clone();
        drop(tx);
        // Still one sender alive.
        assert_eq!(rx.try_recv(), Ok(false));
        drop(tx2);
        assert_eq!(rx.recv(), Err(Disconnected));
    }

    #[test]
    fn try_recv_reports_disconnect() {
        let (tx, rx) = channel();
        drop(tx);
        assert_eq!(rx.try_recv(), Err(Disconnected));
    }

    #[test]
    fn notification_takes_priority_over_disconnect() {
        let (tx, rx) = channel();
        tx.notify();
        drop(tx);
        // Notification delivered first despite disconnect.
        assert_eq!(rx.recv(), Ok(()));
        // Now disconnected.
        assert_eq!(rx.recv(), Err(Disconnected));
    }

    #[test]
    fn recv_unblocks_on_disconnect() {
        let (tx, rx) = channel();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(tx);
        });
        assert_eq!(rx.recv(), Err(Disconnected));
        handle.join().expect("sender thread panicked");
    }
}
