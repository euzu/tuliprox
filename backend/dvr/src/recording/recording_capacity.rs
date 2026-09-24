//! What the DVR needs from whatever owns provider connections.
//!
//! The DVR decides whether to record, what to write and when to stop. It does
//! not decide whether there is a connection available to do it with, and it
//! must not need a real provider to be tested. This is the seam: the app
//! adapts its connection managers to it, and tests supply a stand-in.

use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Notify;
use tuliprox_core::model::ProviderHandle;

/// One provider's capacity for an input: name, connections in use, and limit.
///
/// A limit of zero means unlimited.
pub type ProviderCapacity = (Arc<str>, usize, usize);

/// Provider capacity, as the recording worker sees it.
///
/// Object-safe on purpose -- `RecordingCtx` holds one of these behind `dyn`, so
/// the whole DVR does not become generic over its provider.
pub trait RecordingCapacityPort: Send + Sync {
    /// What every provider serving this input currently looks like.
    fn capacities_for_input<'a>(&'a self, input_name: &'a Arc<str>) -> BoxFuture<'a, Vec<ProviderCapacity>>;

    /// Take a connection slot, or `None` when none is free.
    ///
    /// Lower priority values are stronger, matching the queue.
    fn acquire<'a>(&'a self, input_name: &'a Arc<str>, priority: i8) -> BoxFuture<'a, Option<ProviderHandle>>;

    /// Give a slot back. Accepts `None` so callers can release unconditionally
    /// on paths where they may never have held one.
    fn release(&self, handle: Option<ProviderHandle>) -> BoxFuture<'_, ()>;

    /// Fires when a connection is freed anywhere, so waiters can re-check.
    fn capacity_changed(&self) -> Arc<Notify>;
}

#[cfg(test)]
pub mod stub {
    //! A capacity port that answers from a script, for testing the worker
    //! without a provider.

    use super::{ProviderCapacity, RecordingCapacityPort};
    use futures::future::BoxFuture;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::Notify;
    use tuliprox_core::model::ProviderHandle;

    /// Counts what the worker asked for, and hands out slots on request.
    pub struct StubCapacity {
        capacities: Mutex<Vec<ProviderCapacity>>,
        notify: Arc<Notify>,
        pub acquires: AtomicUsize,
        pub releases: AtomicUsize,
    }

    /// The recording worker reads nothing off a handle but its
    /// `cancel_token`, so the allocation carried here is a placeholder and
    /// not a claim that some real provider slot is backing it.
    fn granted_handle() -> ProviderHandle {
        ProviderHandle::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            1,
            tuliprox_core::model::ProviderAllocation::Exhausted,
            Some(tokio_util::sync::CancellationToken::new()),
        )
    }

    impl StubCapacity {
        fn with_capacity(in_use: usize, limit: usize) -> Arc<Self> {
            Arc::new(Self {
                capacities: Mutex::new(vec![(Arc::from("provider"), in_use, limit)]),
                notify: Arc::new(Notify::new()),
                acquires: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
            })
        }

        /// A provider with room.
        pub fn with_room() -> Arc<Self> { Self::with_capacity(0, 4) }

        /// A provider that is full, so every request has to wait.
        pub fn full() -> Arc<Self> { Self::with_capacity(4, 4) }

        /// Whether the reported capacities leave anything to hand out. Kept
        /// consistent with what `capacities_for_input` says, so the stub
        /// cannot advertise headroom and then refuse to grant it.
        fn has_room(&self) -> bool {
            self.capacities.lock().expect("capacities").iter().any(|(_, in_use, limit)| *limit == 0 || in_use < limit)
        }

        pub fn acquire_count(&self) -> usize { self.acquires.load(Ordering::SeqCst) }

        pub fn release_count(&self) -> usize { self.releases.load(Ordering::SeqCst) }
    }

    impl RecordingCapacityPort for StubCapacity {
        fn capacities_for_input<'a>(&'a self, _input_name: &'a Arc<str>) -> BoxFuture<'a, Vec<ProviderCapacity>> {
            Box::pin(async move { self.capacities.lock().expect("capacities").clone() })
        }

        fn acquire<'a>(&'a self, _input_name: &'a Arc<str>, _priority: i8) -> BoxFuture<'a, Option<ProviderHandle>> {
            Box::pin(async move {
                self.acquires.fetch_add(1, Ordering::SeqCst);
                self.has_room().then(granted_handle)
            })
        }

        fn release(&self, _handle: Option<ProviderHandle>) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.releases.fetch_add(1, Ordering::SeqCst);
            })
        }

        fn capacity_changed(&self) -> Arc<Notify> { Arc::clone(&self.notify) }
    }
}

#[cfg(test)]
mod tests {
    use super::{stub::StubCapacity, RecordingCapacityPort};
    use std::sync::Arc;

    #[tokio::test]
    async fn a_full_provider_offers_no_room_and_grants_nothing() {
        // This is what makes a worker wait, and until now it could only be
        // produced by a real provider actually being full.
        let capacity = StubCapacity::full();
        let input: Arc<str> = Arc::from("provider");

        let reported = capacity.capacities_for_input(&input).await;
        assert_eq!(reported, vec![(Arc::from("provider"), 4, 4)], "no headroom");
        assert!(capacity.acquire(&input, 0).await.is_none(), "and nothing to hand out");
        assert_eq!(capacity.acquire_count(), 1, "the attempt is observable, which is the point");
    }

    #[tokio::test]
    async fn a_provider_with_room_grants_what_it_advertises() {
        // The stub must not report headroom and then refuse to hand a slot
        // out: a worker that believes the first and hits the second waits
        // forever, and the test that set it up looks like a product bug.
        let capacity = StubCapacity::with_room();
        let input: Arc<str> = Arc::from("provider");

        assert_eq!(capacity.capacities_for_input(&input).await, vec![(Arc::from("provider"), 0, 4)], "headroom");
        assert!(capacity.acquire(&input, 0).await.is_some(), "so a slot is actually granted");
    }

    #[tokio::test]
    async fn releasing_is_counted_even_when_no_slot_was_held() {
        // The worker releases unconditionally on paths where it may never have
        // acquired. That has to be harmless, and countable, so a leak or a
        // double release is something a test can see.
        let capacity = StubCapacity::with_room();
        capacity.release(None).await;
        capacity.release(None).await;
        assert_eq!(capacity.release_count(), 2);
    }
}
