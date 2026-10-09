//! Scoped tracing subscribers for tests that capture events.
//!
//! `tracing` caches each callsite's interest process-wide, the first time the
//! callsite is hit. While exactly one dispatcher is registered, `tracing-core`
//! computes that interest from the *calling thread's* default rather than from
//! the registered dispatcher. So when a test's scoped capture subscriber is the
//! only one alive and another test thread, with no subscriber, reaches the same
//! callsite first, the callsite is cached as `Interest::never()` until the next
//! dispatcher registers anywhere — and the capture, already registered, sees no
//! event at all (#369).
//!
//! [`with_default`] and [`set_default`] first pin two permanent no-op
//! dispatchers. Registering them rebuilds every callsite already cached, and
//! from then on the registered set never drops to one. `tracing-core` then
//! always computes interest under its dispatcher lock, from every registered
//! dispatcher, which includes every live capture subscriber on any thread.
//! The pins register on the first capture, not at process start, so a thread
//! already mid-registration on the unlocked path at that moment can still
//! store a stale `never` after the pins' rebuild; it cannot happen afterwards.
//! Every test that captures events installs its subscriber through here.

use std::sync::OnceLock;

use tracing::subscriber::{DefaultGuard, NoSubscriber};
use tracing::{Dispatch, Subscriber};

fn pin_dispatchers() {
    static PINNED: OnceLock<[Dispatch; 2]> = OnceLock::new();
    PINNED.get_or_init(|| {
        [
            Dispatch::new(NoSubscriber::default()),
            Dispatch::new(NoSubscriber::default()),
        ]
    });
}

/// [`tracing::subscriber::with_default`], safe against other test threads.
pub(crate) fn with_default<T, S>(subscriber: S, f: impl FnOnce() -> T) -> T
where
    S: Subscriber + Send + Sync + 'static,
{
    pin_dispatchers();
    tracing::subscriber::with_default(subscriber, f)
}

/// [`tracing::subscriber::set_default`], safe against other test threads.
pub(crate) fn set_default<S>(subscriber: S) -> DefaultGuard
where
    S: Subscriber + Send + Sync + 'static,
{
    pin_dispatchers();
    tracing::subscriber::set_default(subscriber)
}

mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    struct CountEvents(Arc<Mutex<usize>>);
    impl<S: Subscriber> Layer<S> for CountEvents {
        fn on_event(&self, _event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            *self.0.lock().unwrap() += 1;
        }
    }

    fn emit() {
        tracing::info!(event = "test_tracing.probe", "probe");
    }

    /// Another thread with no subscriber reaches the callsite first, while
    /// this thread's capture is live — the ordering that cached the callsite
    /// as `never` and left the capture empty (#369).
    #[test]
    fn a_capture_sees_an_event_another_thread_hit_first_with_no_subscriber() {
        let seen = Arc::new(Mutex::new(0));
        let subscriber = tracing_subscriber::registry().with(CountEvents(Arc::clone(&seen)));
        super::with_default(subscriber, || {
            std::thread::spawn(emit).join().unwrap();
            emit();
        });
        assert_eq!(*seen.lock().unwrap(), 1);
    }
}
