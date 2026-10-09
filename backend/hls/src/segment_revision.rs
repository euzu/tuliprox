use super::{CachedSegmentMetadata, HlsCacheObjectKey, ProxySessionId, SegmentCacheKey};
use crate::sync_ext::MutexExt;
use shared::model::HlsStartupMode;
use std::{
    collections::BTreeMap,
    fmt, io,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum SegmentRevisionKind {
    Raw,
    Processed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct SegmentRevisionId {
    generation: u64,
    ordinal: u64,
}

#[derive(Clone, Debug)]
pub struct SegmentRevisionKey {
    session: ProxySessionId,
    pub proxy_seq: u64,
    id: SegmentRevisionId,
    pub kind: SegmentRevisionKind,
}

impl HlsCacheObjectKey for SegmentRevisionKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session }
    fn session_path_component(&self) -> String {
        SegmentCacheKey::new(self.session.clone(), self.proxy_seq, "ts").session_path_component()
    }
    fn file_name(&self) -> String {
        let kind = match self.kind {
            SegmentRevisionKind::Raw => "raw",
            SegmentRevisionKind::Processed => "processed",
        };
        format!("revision-{:016x}-{:016x}-{kind}.ts", self.id.generation, self.id.ordinal)
    }
}

#[derive(Debug, Clone)]
pub enum SegmentRevisionState {
    Pending,
    Complete(CachedSegmentMetadata),
    Failed,
}

pub struct SegmentRevision {
    pub key: SegmentRevisionKey,
    pins: AtomicUsize,
    pub prefix_available: AtomicU64,
    state: watch::Sender<SegmentRevisionState>,
    exposed: AtomicBool,
    pub(crate) file_pin: Mutex<Option<super::HlsRevisionFilePin>>,
    pub(crate) replay: Mutex<Option<super::progressive_startup::ProgressiveReplay>>,
}

impl fmt::Debug for SegmentRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentRevision")
            .field("id", &self.key.id)
            .field("kind", &self.key.kind)
            .field("pins", &self.pins.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl SegmentRevision {
    pub fn id(&self) -> SegmentRevisionId { self.key.id }
    #[cfg(test)]
    pub(crate) fn pin_count(&self) -> usize { self.pins.load(Ordering::Acquire) }
    pub fn subscribe(&self) -> watch::Receiver<SegmentRevisionState> { self.state.subscribe() }
    pub fn complete(&self, metadata: CachedSegmentMetadata) -> bool {
        self.state.send_if_modified(|state| {
            if !matches!(state, SegmentRevisionState::Pending) {
                return false;
            }
            *state = SegmentRevisionState::Complete(metadata);
            true
        })
    }
    pub(crate) fn prefix_changed(&self) { self.state.send_modify(|_| {}); }
    fn is_exposable(&self, state: &SegmentRevisionState) -> bool {
        match state {
            SegmentRevisionState::Complete(_) => true,
            SegmentRevisionState::Pending => {
                self.prefix_available.load(Ordering::Acquire) > 0 && self.replay.lock_unpoisoned().is_some()
            }
            SegmentRevisionState::Failed => false,
        }
    }

    /// Side-effect-free precheck for [`Self::expose`].
    pub(crate) fn can_expose(&self) -> bool { self.is_exposable(&self.state.borrow()) }

    pub(crate) fn expose(&self) -> bool {
        let mut exposed = false;
        self.state.send_if_modified(|state| {
            exposed = self.is_exposable(state);
            if exposed {
                self.exposed.store(true, Ordering::Release);
            }
            false
        });
        exposed
    }

    pub(crate) fn replay_exhausted(&self) {
        self.state.send_if_modified(|state| {
            if !matches!(state, SegmentRevisionState::Pending) {
                return false;
            }
            self.replay.lock_unpoisoned().take();
            if self.exposed.load(Ordering::Acquire) {
                *state = SegmentRevisionState::Failed;
            } else {
                self.prefix_available.store(0, Ordering::Release);
            }
            true
        });
    }

    pub fn fail(&self) {
        self.state.send_if_modified(|state| {
            if !matches!(state, SegmentRevisionState::Pending) {
                return false;
            }
            *state = SegmentRevisionState::Failed;
            true
        });
        self.replay.lock_unpoisoned().take();
    }
    pub fn state(&self) -> SegmentRevisionState { self.state.borrow().clone() }
    pub fn is_complete(&self) -> bool { matches!(*self.state.borrow(), SegmentRevisionState::Complete(_)) }

    /// Whether this revision may open the startup window as its head.
    pub fn is_startup_head_ready(&self, mode: HlsStartupMode) -> bool {
        match mode {
            HlsStartupMode::FirstReady => self.is_complete(),
            HlsStartupMode::Progressive => self.is_publishable(),
            HlsStartupMode::Conservative => false,
        }
    }

    pub fn is_publishable(&self) -> bool {
        match &*self.state.borrow() {
            SegmentRevisionState::Failed => false,
            SegmentRevisionState::Complete(_) => true,
            SegmentRevisionState::Pending => self.prefix_available.load(Ordering::Acquire) > 0,
        }
    }
    pub async fn wait_complete(&self, deadline: tokio::time::Instant) -> io::Result<CachedSegmentMetadata> {
        let mut state = self.subscribe();
        loop {
            match state.borrow_and_update().clone() {
                SegmentRevisionState::Complete(metadata) => return Ok(metadata),
                SegmentRevisionState::Failed => return Err(io::Error::other("published segment revision failed")),
                SegmentRevisionState::Pending => {}
            }
            tokio::time::timeout_at(deadline, state.changed())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "segment revision wait timed out"))?
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "segment revision producer ended"))?;
        }
    }
}

#[derive(Debug)]
pub struct SegmentRevisionGuard {
    revision: Arc<SegmentRevision>,
}

impl SegmentRevisionGuard {
    pub fn revision(&self) -> &SegmentRevision { &self.revision }
}
impl Clone for SegmentRevisionGuard {
    fn clone(&self) -> Self {
        self.revision.pins.fetch_add(1, Ordering::AcqRel);
        Self { revision: Arc::clone(&self.revision) }
    }
}
impl Drop for SegmentRevisionGuard {
    fn drop(&mut self) { self.revision.pins.fetch_sub(1, Ordering::AcqRel); }
}
impl PartialEq for SegmentRevisionGuard {
    fn eq(&self, other: &Self) -> bool { self.revision.id() == other.revision.id() }
}
impl Eq for SegmentRevisionGuard {}

#[derive(Debug)]
pub struct SegmentRevisionStore {
    generation: u64,
    next: AtomicU64,
    entries: Mutex<BTreeMap<SegmentRevisionId, Arc<SegmentRevision>>>,
}

impl Default for SegmentRevisionStore {
    fn default() -> Self {
        Self { generation: fastrand::u64(..), next: AtomicU64::new(0), entries: Mutex::new(BTreeMap::new()) }
    }
}

impl SegmentRevisionStore {
    pub(crate) fn has_created_revisions(&self) -> bool { self.next.load(Ordering::Acquire) > 0 }
    pub fn create(
        &self,
        session: ProxySessionId,
        proxy_seq: u64,
        kind: SegmentRevisionKind,
    ) -> io::Result<SegmentRevisionGuard> {
        let mut ordinal = self.next.load(Ordering::Acquire);
        loop {
            let next = ordinal.checked_add(1).ok_or_else(|| io::Error::other("segment revision identity exhausted"))?;
            match self.next.compare_exchange_weak(ordinal, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => ordinal = current,
            }
        }
        let id = SegmentRevisionId { generation: self.generation, ordinal };
        let revision = Arc::new(SegmentRevision {
            key: SegmentRevisionKey { session, proxy_seq, id, kind },
            pins: AtomicUsize::new(1),
            prefix_available: AtomicU64::new(0),
            state: watch::channel(SegmentRevisionState::Pending).0,
            exposed: AtomicBool::new(false),
            file_pin: Mutex::new(None),
            replay: Mutex::new(None),
        });
        self.entries
            .lock()
            .map_err(|_| io::Error::other("revision registry poisoned"))?
            .insert(id, Arc::clone(&revision));
        Ok(SegmentRevisionGuard { revision })
    }

    pub fn requeue_retired(&self, revision: Arc<SegmentRevision>) -> io::Result<()> {
        self.entries
            .lock()
            .map_err(|_| io::Error::other("revision registry poisoned"))?
            .insert(revision.id(), revision);
        Ok(())
    }

    pub fn retire_unpinned(&self) -> io::Result<Vec<Arc<SegmentRevision>>> {
        let mut entries = self.entries.lock().map_err(|_| io::Error::other("revision registry poisoned"))?;
        let ids: Vec<_> = entries
            .iter()
            .filter_map(|(id, revision)| (revision.pins.load(Ordering::Acquire) == 0).then_some(*id))
            .take(128)
            .collect();
        Ok(ids.into_iter().filter_map(|id| entries.remove(&id)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn published_owner_cannot_be_replaced_by_a_new_attempt() -> io::Result<()> {
        let store = SegmentRevisionStore::default();
        let published = store.create(ProxySessionId("session".into()), 7, SegmentRevisionKind::Processed)?;
        let next = store.create(ProxySessionId("session".into()), 7, SegmentRevisionKind::Processed)?;
        assert_ne!(published.revision().id(), next.revision().id());
        published.revision().fail();
        assert!(next.revision().complete(CachedSegmentMetadata { path: "next.ts".into(), size: 10 }));
        assert!(published.revision().wait_complete(tokio::time::Instant::now()).await.is_err());
        assert!(!published.revision().complete(CachedSegmentMetadata { path: "wrong.ts".into(), size: 10 }));
        Ok(())
    }

    #[test]
    fn replay_exhaustion_before_exposure_waits_for_the_identical_spool() -> io::Result<()> {
        let store = SegmentRevisionStore::default();
        let owner = store.create(ProxySessionId("budget".into()), 0, SegmentRevisionKind::Raw)?;
        let budget = super::super::ProgressiveBudgetManager::new(tuliprox_core::model::HlsStartupConfig::default());
        let mut replay = super::super::progressive_startup::ProgressiveReplay::new(&budget)
            .ok_or_else(|| io::Error::other("slot"))?;
        replay.push(b"prefix")?;
        *owner.revision().replay.lock_unpoisoned() = Some(replay);
        owner.revision().prefix_available.store(6, Ordering::Release);
        owner.revision().replay_exhausted();
        assert!(matches!(*owner.revision().subscribe().borrow(), SegmentRevisionState::Pending));
        assert!(!owner.revision().is_publishable());
        assert!(!owner.revision().expose());
        assert_eq!(budget.usage(), (0, 0));
        assert!(owner.revision().complete(CachedSegmentMetadata { path: "raw.ts".into(), size: 6 }));
        assert!(owner.revision().expose());
        Ok(())
    }

    #[test]
    fn exposed_replay_exhaustion_is_terminal() -> io::Result<()> {
        let store = SegmentRevisionStore::default();
        let owner = store.create(ProxySessionId("budget".into()), 0, SegmentRevisionKind::Raw)?;
        let budget = super::super::ProgressiveBudgetManager::new(tuliprox_core::model::HlsStartupConfig::default());
        let mut replay = super::super::progressive_startup::ProgressiveReplay::new(&budget)
            .ok_or_else(|| io::Error::other("slot"))?;
        replay.push(b"prefix")?;
        *owner.revision().replay.lock_unpoisoned() = Some(replay);
        owner.revision().prefix_available.store(6, Ordering::Release);
        assert!(owner.revision().expose());
        owner.revision().replay_exhausted();
        assert!(matches!(*owner.revision().subscribe().borrow(), SegmentRevisionState::Failed));
        assert!(!owner.revision().complete(CachedSegmentMetadata { path: "raw.ts".into(), size: 6 }));
        assert_eq!(budget.usage(), (0, 0));
        Ok(())
    }

    #[test]
    fn repeated_retention_and_reader_churn_leaves_no_revision_owners() -> io::Result<()> {
        let store = SegmentRevisionStore::default();
        let mut window = std::collections::VecDeque::new();
        for seq in 0..10_000 {
            let owner = store.create(ProxySessionId("churn".into()), seq, SegmentRevisionKind::Processed)?;
            let readers: Vec<_> = (0..16).map(|_| owner.clone()).collect();
            window.push_back(owner);
            if window.len() > 6 {
                window.pop_front();
            }
            drop(readers);
            drop(store.retire_unpinned()?);
            assert_eq!(store.entries.lock().map_err(|_| io::Error::other("registry"))?.len(), window.len());
        }
        drop(window);
        drop(store.retire_unpinned()?);
        assert!(store.entries.lock().map_err(|_| io::Error::other("registry"))?.is_empty());
        Ok(())
    }

    #[test]
    fn retirement_waits_for_all_explicit_guards() -> io::Result<()> {
        let store = SegmentRevisionStore::default();
        let guard = store.create(ProxySessionId("session".into()), 7, SegmentRevisionKind::Raw)?;
        let reader = guard.clone();
        drop(guard);
        assert!(store.retire_unpinned()?.is_empty());
        drop(reader);
        assert_eq!(store.retire_unpinned()?.len(), 1);
        assert!(store.retire_unpinned()?.is_empty());
        Ok(())
    }
}
