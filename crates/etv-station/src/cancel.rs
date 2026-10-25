//! A shutdown request that both blocking and async code can observe.
//!
//! The daemon's `Notify` signals hand one permit to one waiter, which is right
//! for a loop that owns the signal but wrong for work running elsewhere: the
//! Plex ingest runs on a blocking thread and a multi-minute pass has to learn
//! about SIGTERM without being the one waiter. [`Cancel`] is a flag the
//! blocking side polls plus a wake-up the async side awaits.

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    /// Request cancellation and wake every task parked in [`Self::cancelled`].
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// The flag itself, for blocking code that takes an `&AtomicBool`.
    pub fn flag(&self) -> &AtomicBool {
        &self.flag
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`Self::cancel`] has been called, including before this
    /// future was first polled.
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Register for wake-ups before reading the flag, so a `cancel` landing
        // between the two cannot be missed.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
