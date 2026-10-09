use crate::{provider_leases::ProviderLeaseTable, provider_lineup_manager::ProviderLineupManager, SharedStreamManager};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, LazyLock, OnceLock, Weak,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{
    AllocationId, PlaybackKind, PlaybackRequestId, ProviderAllocation, ProviderHandle, SharedSubscriberId,
};

static DUMMY_ADDR: LazyLock<SocketAddr> = LazyLock::new(|| SocketAddr::from(([127, 0, 0, 1], 0)));

const PREEMPTION_COMPLETION_TIMEOUT: Duration = Duration::from_millis(1500);

const EVICTED_PROVIDER_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProviderReleaseSnapshot {
    pub addr: SocketAddr,
    single_allocations: Vec<AllocationId>,
    shared_subscribers: Vec<SharedSubscriberId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Normal,
    Soft,
}

/// Playback identity of one provider acquisition.
///
/// `owner` carries the public session identity so transport cleanup can distinguish
/// retries. Provider leases use [`Self::provider_owner`] to resolve it to a stable
/// client/user/channel owner and therefore one capacity slot.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackLeaseRef<'a> {
    pub owner: &'a str,
    pub kind: PlaybackKind,
    pub request_id: PlaybackRequestId,
}

/// Tables that record which providers' capacity a write freed.
trait CapacityReleases {
    fn take_released_providers(&mut self) -> Vec<Arc<str>>;
}

/// Wakes only the requests waiting for capacity on a provider whose slot or lease was freed.
#[derive(Default)]
pub struct ProviderCapacityNotifier {
    waiters: std::sync::Mutex<HashMap<Arc<str>, Arc<tokio::sync::Notify>>>,
    staged: std::sync::Mutex<Vec<Arc<str>>>,
    has_staged: AtomicBool,
}

pub struct ManagedProviderHandle {
    manager: Arc<ActiveProviderManager>,
    handle: Option<ProviderHandle>,
}

pub struct ActiveProviderManagerCore {
    // Serializes capacity transitions across counters, allocation indices and leases.
    capacity_transition: std::sync::Mutex<()>,
    is_shutting_down: AtomicBool,
    shutdown_token: CancellationToken,
    pub(crate) providers: ProviderLineupManager,
    connections: std::sync::RwLock<Connections>,
    leases: std::sync::RwLock<ProviderLeaseTable>,
    next_allocation_id: AtomicU64,
    // Wakes the capacity waiters of a provider when one of its slots or leases is freed.
    capacity: ProviderCapacityNotifier,
}

#[derive(Debug)]
pub(crate) enum PreemptionOutcome {
    Acquired(ProviderAllocation),
    /// Victim body is still draining. The optional pair is `(alloc_id, open_generation)` of the
    /// victim for an idempotent self-release attempt once the token fires; `None` for shared-stream
    /// teardowns where release happens inside the spawned task and no `complete_release_locked` call
    /// is needed.
    PendingCompletion(Option<(AllocationId, u64)>, CancellationToken),
    Exhausted,
}

pub struct ActiveProviderManager {
    core: Arc<ActiveProviderManagerCore>,
    shared_stream_manager: OnceLock<Weak<SharedStreamManager>>,
}

#[cfg(test)]
mod tests;

mod account_health;
mod allocation;
mod capacity;
mod leases;
mod preemption;
mod release;
mod shared;

use self::{
    allocation::{playback_lease_owner, AcquireProviderParams, Connections},
    leases::OwnerProviderPin,
    preemption::{PriorityKey, PriorityOwner},
    release::{ProviderAllocationGuard, ReleaseAction},
    shared::{SharedConnectionId, SharedConnections},
};
