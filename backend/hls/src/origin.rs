use super::{hls_ctx::HlsCtx, HlsSession, HlsSessionHandle, HlsSessionKey, ProxySessionId};
use log::debug;
use shared::utils::sanitize_sensitive_info;
use std::{
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Notify, time::timeout};
use tuliprox_core::model::{
    is_input_expired, PlaybackKind, PlaybackRequestId, ProviderAllocation, ProviderBindingTag, ProviderHandle,
};
use tuliprox_session::{CleanupEvent, ConnectionKind, PlaybackLeaseRef};

const HLS_ACCOUNT_OVERLAP_FALLBACK_TARGET_DURATION_MS: u64 = 15_000;
const HLS_ORIGIN_ACCOUNT_IO_WAIT_RECHECK: Duration = Duration::from_millis(25);

/// Stable source metadata for one shared live-HLS session.
///
/// Session identity is derived only from `input_id` and the immutable input
/// origin ID in `stream_ref`; target IDs, virtual IDs, and resolved origin URLs
/// are deliberately excluded.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsOriginSource {
    /// Internal ID of the configured Tuliprox input.
    pub input_id: u16,
    /// Input name used to resolve the current origin account or alias.
    pub input_name: Arc<str>,
    /// Exact, non-empty origin/provider stream ID captured before target mapping.
    pub stream_ref: String,
    /// Archive start timestamp; absent for live playback.
    pub archive_reference: Option<i64>,
    /// Opaque identity of the complete archive request; absent for live playback.
    pub archive_identity: Option<String>,
    pub source_kind: HlsOriginSourceKind,
}

impl HlsOriginSource {
    /// Creates source metadata from an immutable input-stream identity.
    ///
    /// `stream_ref` must be the origin/provider ID, never a target-specific or
    /// virtual ID.
    pub fn new(
        input_id: u16,
        input_name: Arc<str>,
        stream_ref: impl Into<String>,
        source_kind: HlsOriginSourceKind,
    ) -> Self {
        Self {
            input_id,
            input_name,
            stream_ref: stream_ref.into(),
            archive_reference: None,
            archive_identity: None,
            source_kind,
        }
    }

    pub fn from_session_key(key: &HlsSessionKey) -> Self {
        let mut source =
            Self::new(key.input_id, Arc::from(""), key.stream_ref.clone(), HlsOriginSourceKind::DirectMediaPlaylist);
        source.archive_reference = key.archive_reference;
        source.archive_identity.clone_from(&key.archive_identity);
        source
    }

    pub const fn with_archive_reference(mut self, archive_reference: i64) -> Self {
        self.archive_reference = Some(archive_reference);
        self
    }

    pub fn with_archive_request(mut self, archive_reference: i64, archive_url: &str) -> Self {
        self.archive_reference = Some(archive_reference);
        self.archive_identity = Some(blake3::hash(archive_url.as_bytes()).to_hex().to_string());
        self
    }

    pub fn session_key(&self) -> HlsSessionKey {
        let key = HlsSessionKey::new(self.input_id, self.stream_ref.clone());
        match self.archive_reference {
            Some(timestamp) => match self.archive_identity.as_ref() {
                Some(identity) => key.with_archive_reference(timestamp).with_archive_identity(identity),
                None => key.with_archive_reference(timestamp),
            },
            None => key,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsOriginSourceKind {
    XtreamLive,
    M3uMediaPlaylist,
    DirectMediaPlaylist,
}

/// Tuliprox input/alias account binding for one shared HLS content session.
#[derive(Clone)]
pub struct HlsOriginAccountBinding {
    pub input_name: Arc<str>,
    pub account_name: Arc<str>,
    pub session_owner: String,
    pub playback_request_id: Option<PlaybackRequestId>,
    pub pinned_at_ms: u64,
    pub last_origin_io_at_ms: Option<u64>,
    pub last_reservation_refresh_at_ms: Option<u64>,
    pub binding_mode: HlsOriginAccountBindingMode,
    pub generation: u64,
    /// The provider lease's binding tag captured when this binding acquired its slot.
    /// A stale detach/clear carries the old tag and cannot delete a successor lease
    /// on the same account (A→A rebind or remove/recreate).
    pub provider_binding_tag: Option<ProviderBindingTag>,
}

pub struct HlsOriginAccountIoLease {
    pub account_name: Arc<str>,
    pub session_owner: String,
    pub generation: u64,
    pub active_io_count: std::sync::atomic::AtomicUsize,
    provider_handle: std::sync::Mutex<Option<ProviderHandle>>,
    cleanup_permit: std::sync::Mutex<Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>>,
    acquiring: std::sync::atomic::AtomicBool,
    notify: Arc<Notify>,
}

impl HlsOriginAccountIoLease {
    fn acquiring(binding: &HlsOriginAccountBinding) -> Self {
        Self {
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            generation: binding.generation,
            active_io_count: std::sync::atomic::AtomicUsize::new(0),
            provider_handle: std::sync::Mutex::new(None),
            cleanup_permit: std::sync::Mutex::new(None),
            acquiring: std::sync::atomic::AtomicBool::new(true),
            notify: Arc::new(Notify::new()),
        }
    }

    #[cfg(test)]
    pub fn active_for_test(binding: &HlsOriginAccountBinding, active_io_count: usize) -> Self {
        Self {
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            generation: binding.generation,
            active_io_count: std::sync::atomic::AtomicUsize::new(active_io_count),
            provider_handle: std::sync::Mutex::new(None),
            cleanup_permit: std::sync::Mutex::new(None),
            acquiring: std::sync::atomic::AtomicBool::new(false),
            notify: Arc::new(Notify::new()),
        }
    }

    fn matches_binding(&self, binding: &HlsOriginAccountBinding) -> bool {
        self.account_name == binding.account_name
            && self.session_owner == binding.session_owner
            && self.generation == binding.generation
    }

    pub fn is_active_or_acquiring(&self) -> bool {
        self.active_io_count.load(std::sync::atomic::Ordering::Acquire) > 0
            || self.acquiring.load(std::sync::atomic::Ordering::Acquire)
    }

    fn try_join(&self) -> bool {
        if self.acquiring.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let mut count = self.active_io_count.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if count == 0 {
                return false;
            }
            let Some(next) = count.checked_add(1) else { return false };
            match self.active_io_count.compare_exchange_weak(
                count,
                next,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => count = observed,
            }
        }
    }

    fn take_resource(&self) -> Option<(ProviderHandle, tokio::sync::mpsc::OwnedPermit<CleanupEvent>)> {
        let handle = match self.provider_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }?;
        let permit = match self.cleanup_permit.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }?;
        Some((handle, permit))
    }
}

impl fmt::Debug for HlsOriginAccountIoLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlsOriginAccountIoLease")
            .field("account_name", &self.account_name)
            .field("session_owner", &"<redacted>")
            .field("generation", &self.generation)
            .field("active_io_count", &self.active_io_count.load(std::sync::atomic::Ordering::Relaxed))
            .field("has_provider_handle", &self.provider_handle.lock().is_ok_and(|guard| guard.is_some()))
            .field("acquiring", &self.acquiring.load(std::sync::atomic::Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

pub struct HlsOriginAccountIoLeaseGuard {
    binding: HlsOriginAccountBinding,
    lease: Arc<HlsOriginAccountIoLease>,
    disarmed: bool,
}

impl fmt::Debug for HlsOriginAccountIoLeaseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlsOriginAccountIoLeaseGuard")
            .field("binding", &self.binding)
            .field("disarmed", &self.disarmed)
            .finish_non_exhaustive()
    }
}

impl HlsOriginAccountIoLeaseGuard {
    pub fn new(binding: HlsOriginAccountBinding, lease: Arc<HlsOriginAccountIoLease>) -> Self {
        Self { binding, lease, disarmed: false }
    }

    pub fn binding(&self) -> &HlsOriginAccountBinding { &self.binding }

    pub fn disarm(&mut self) { self.disarmed = true; }
}

impl Drop for HlsOriginAccountIoLeaseGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.disarmed = true;
        if self.lease.active_io_count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            if let Some((handle, permit)) = self.lease.take_resource() {
                permit.send(CleanupEvent::ReleaseProviderHandle { handle: Some(handle) });
            }
        }
    }
}

struct ManagedHandleGuard {
    handle: Option<ProviderHandle>,
    cleanup_permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
}

impl ManagedHandleGuard {
    fn new(handle: ProviderHandle, cleanup_permit: tokio::sync::mpsc::OwnedPermit<CleanupEvent>) -> Self {
        Self { handle: Some(handle), cleanup_permit: Some(cleanup_permit) }
    }

    fn take_resource(&mut self) -> Option<(ProviderHandle, tokio::sync::mpsc::OwnedPermit<CleanupEvent>)> {
        Some((self.handle.take()?, self.cleanup_permit.take()?))
    }
}

impl Drop for ManagedHandleGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Some(permit) = self.cleanup_permit.take() {
                permit.send(CleanupEvent::ReleaseProviderHandle { handle: Some(handle) });
            }
        }
    }
}

pub struct PendingAcquireGuard {
    lease: Arc<HlsOriginAccountIoLease>,
    disarmed: bool,
}

impl PendingAcquireGuard {
    fn new(lease: Arc<HlsOriginAccountIoLease>) -> Self { Self { lease, disarmed: false } }

    fn disarm(&mut self) { self.disarmed = true; }
}

impl Drop for PendingAcquireGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.disarmed = true;
        self.lease.acquiring.store(false, std::sync::atomic::Ordering::Release);
        self.lease.notify.notify_waiters();
    }
}

pub struct HlsAccountOverlapTiming {
    pub target_duration_ms: u64,
    pub hard_active_window_ms: u64,
    pub soft_active_window_ms: u64,
}

impl HlsAccountOverlapTiming {
    pub fn from_target_duration_secs(target_duration_secs: Option<u64>) -> Self {
        let target_duration_ms = target_duration_secs
            .map_or(HLS_ACCOUNT_OVERLAP_FALLBACK_TARGET_DURATION_MS, |duration| duration.saturating_mul(1_000));
        Self {
            target_duration_ms,
            hard_active_window_ms: target_duration_ms,
            soft_active_window_ms: target_duration_ms.saturating_mul(2),
        }
    }

    pub fn reservation_ttl_secs(self) -> u64 {
        self.hard_active_window_ms.saturating_add(self.soft_active_window_ms).saturating_add(999) / 1_000 + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsAccountBindingProtection {
    NoMediaYet,
    HardActive { until_ms: u64 },
    SoftActive { reclaim_until_ms: u64 },
    Expired,
}

impl HlsAccountBindingProtection {
    pub const fn as_log_state(self) -> &'static str {
        match self {
            Self::NoMediaYet => "no-media-yet",
            Self::HardActive { .. } => "hard",
            Self::SoftActive { .. } => "soft",
            Self::Expired => "expired",
        }
    }
}

pub fn classify_account_binding_protection(
    last_authorized_media_at_ms: Option<u64>,
    now_ms: u64,
    timing: &HlsAccountOverlapTiming,
) -> HlsAccountBindingProtection {
    let Some(last_media) = last_authorized_media_at_ms else {
        return HlsAccountBindingProtection::NoMediaYet;
    };

    let hard_until = last_media.saturating_add(timing.hard_active_window_ms);
    if now_ms <= hard_until {
        return HlsAccountBindingProtection::HardActive { until_ms: hard_until };
    }

    let reclaim_until = hard_until.saturating_add(timing.soft_active_window_ms);
    if now_ms <= reclaim_until {
        return HlsAccountBindingProtection::SoftActive { reclaim_until_ms: reclaim_until };
    }

    HlsAccountBindingProtection::Expired
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsOriginAccountBindingMode {
    Active,
    Speculative { displaced_proxy_session_id: ProxySessionId, reclaim_until_ms: u64 },
    Detached { reason: HlsOriginAccountDetachedReason, detached_at_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginAccountDetachedReason {
    SoftWindowElapsed,
    ReclaimedByOriginalOwner,
    PreemptedByHigherPriority,
    AccountMissingOrExpired,
    IdleNoActiveLease,
    Cleanup,
}

impl HlsOriginAccountDetachedReason {
    pub const fn as_log_reason(self) -> &'static str {
        match self {
            Self::SoftWindowElapsed => "soft-window-elapsed",
            Self::ReclaimedByOriginalOwner => "reclaimed-by-original-owner",
            Self::PreemptedByHigherPriority => "preempted-by-higher-priority",
            Self::AccountMissingOrExpired => "account-missing-or-expired",
            Self::IdleNoActiveLease => "idle-no-active-lease",
            Self::Cleanup => "cleanup",
        }
    }
}

impl HlsOriginAccountBinding {
    pub fn new(input_name: Arc<str>, account_name: Arc<str>, proxy_session_id: &ProxySessionId, now_ms: u64) -> Self {
        Self {
            input_name,
            account_name,
            session_owner: build_hls_origin_session_owner(proxy_session_id),
            playback_request_id: None,
            pinned_at_ms: now_ms,
            last_origin_io_at_ms: None,
            last_reservation_refresh_at_ms: None,
            binding_mode: HlsOriginAccountBindingMode::Active,
            generation: 0,
            provider_binding_tag: None,
        }
    }

    pub fn rebound(
        input_name: Arc<str>,
        account_name: Arc<str>,
        session_owner: String,
        generation: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            input_name,
            account_name,
            session_owner,
            playback_request_id: None,
            pinned_at_ms: now_ms,
            last_origin_io_at_ms: None,
            last_reservation_refresh_at_ms: None,
            binding_mode: HlsOriginAccountBindingMode::Active,
            generation,
            provider_binding_tag: None,
        }
    }

    pub fn speculative_from(
        input_name: Arc<str>,
        account_name: Arc<str>,
        proxy_session_id: &ProxySessionId,
        displaced_proxy_session_id: ProxySessionId,
        reclaim_until_ms: u64,
        now_ms: u64,
    ) -> Self {
        let mut binding = Self::new(input_name, account_name, proxy_session_id, now_ms);
        binding.binding_mode =
            HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms };
        binding
    }

    pub fn promote_to_active(&mut self) { self.binding_mode = HlsOriginAccountBindingMode::Active; }

    pub fn detach(&mut self, reason: HlsOriginAccountDetachedReason, now_ms: u64) {
        self.binding_mode = HlsOriginAccountBindingMode::Detached { reason, detached_at_ms: now_ms };
        self.last_reservation_refresh_at_ms = None;
    }

    pub const fn is_active(&self) -> bool {
        matches!(
            self.binding_mode,
            HlsOriginAccountBindingMode::Active | HlsOriginAccountBindingMode::Speculative { .. }
        )
    }

    pub const fn is_detached(&self) -> bool {
        matches!(self.binding_mode, HlsOriginAccountBindingMode::Detached { .. })
    }
}

impl fmt::Debug for HlsOriginAccountBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlsOriginAccountBinding")
            .field("input_name", &self.input_name)
            .field("account_name", &self.account_name)
            .field("session_owner", &"<redacted>")
            .field("playback_request_id", &self.playback_request_id)
            .field("pinned_at_ms", &self.pinned_at_ms)
            .field("last_origin_io_at_ms", &self.last_origin_io_at_ms)
            .field("last_reservation_refresh_at_ms", &self.last_reservation_refresh_at_ms)
            .field("binding_mode", &self.binding_mode)
            .field("generation", &self.generation)
            .field("provider_binding_tag", &self.provider_binding_tag)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct HlsOriginAccountRebindState {
    pub last_failed_account: Option<Arc<str>>,
    pub last_rebind_attempt_at_ms: Option<u64>,
    pub next_rebind_allowed_at_ms: Option<u64>,
    pub consecutive_rebind_failures: u32,
}

impl HlsOriginAccountRebindState {
    pub fn is_allowed_now(&self, now_ms: u64) -> bool {
        self.next_rebind_allowed_at_ms.is_none_or(|next| now_ms >= next)
    }

    pub fn mark_attempt_started(&mut self, account_name: Arc<str>, now_ms: u64) {
        self.last_failed_account = Some(account_name);
        self.last_rebind_attempt_at_ms = Some(now_ms);
        self.next_rebind_allowed_at_ms = Some(now_ms.saturating_add(2_000));
    }

    pub fn mark_failed(&mut self, now_ms: u64) {
        self.consecutive_rebind_failures = self.consecutive_rebind_failures.saturating_add(1);
        self.next_rebind_allowed_at_ms = Some(now_ms.saturating_add(2_000));
    }

    pub fn mark_success(&mut self) {
        self.consecutive_rebind_failures = 0;
        self.next_rebind_allowed_at_ms = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginAccountStatus {
    Known,
    Missing,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsBoundAccountAcquireErrorKind {
    Missing,
    Expired,
    Exhausted,
    ReservedForOther,
    Detached,
    WaitTimedOut,
    AcquireTimedOut,
    StoreRace,
    Unavailable,
}

impl HlsBoundAccountAcquireErrorKind {
    pub fn allows_rebind(self) -> bool { matches!(self, Self::Missing | Self::Expired) }

    pub fn is_retryable_resource_failure(self) -> bool {
        matches!(
            self,
            Self::Exhausted | Self::WaitTimedOut | Self::AcquireTimedOut | Self::StoreRace | Self::Unavailable
        )
    }

    pub const fn as_log_label(self) -> &'static str {
        match self {
            Self::Missing => "Missing",
            Self::Expired => "Expired",
            Self::Exhausted => "Exhausted",
            Self::ReservedForOther => "ReservedForOther",
            Self::Detached => "Detached",
            Self::WaitTimedOut => "WaitTimedOut",
            Self::AcquireTimedOut => "AcquireTimedOut",
            Self::StoreRace => "StoreRace",
            Self::Unavailable => "Unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsEffectiveOriginAcquirePolicy {
    pub connection_kind: ConnectionKind,
    pub priority: i8,
    pub updated_at_ms: u64,
}

impl HlsEffectiveOriginAcquirePolicy {
    pub const fn new(connection_kind: ConnectionKind, priority: i8, updated_at_ms: u64) -> Self {
        Self { connection_kind, priority, updated_at_ms }
    }

    pub const fn fallback() -> Self { Self::new(ConnectionKind::Normal, 0, 0) }

    pub const fn with_updated_at(self, updated_at_ms: u64) -> Self {
        Self::new(self.connection_kind, self.priority, updated_at_ms)
    }

    pub fn has_same_rank_as(self, other: Self) -> bool {
        self.connection_kind == other.connection_kind && self.priority == other.priority
    }

    pub const fn is_better_than(self, other: Self) -> bool {
        match (self.connection_kind, other.connection_kind) {
            (ConnectionKind::Normal, ConnectionKind::Soft) => true,
            (ConnectionKind::Soft, ConnectionKind::Normal) => false,
            _ => self.priority < other.priority,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsEffectiveOriginAcquirePolicyState {
    pub current_policy: HlsEffectiveOriginAcquirePolicy,
    pub last_supported_at_ms: u64,
}

impl HlsEffectiveOriginAcquirePolicyState {
    pub const fn new(current_policy: HlsEffectiveOriginAcquirePolicy, now_ms: u64) -> Self {
        Self { current_policy, last_supported_at_ms: now_ms }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginWorkClass {
    ManifestInteractive,
    Demand,
    Background,
}

impl HlsOriginWorkClass {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::ManifestInteractive => "manifest_interactive",
            Self::Demand => "demand",
            Self::Background => "background",
        }
    }

    pub const fn allows_grace(self) -> bool { matches!(self, Self::ManifestInteractive | Self::Demand) }

    pub const fn allows_speculative_overlap(self) -> bool { matches!(self, Self::ManifestInteractive | Self::Demand) }
}

#[derive(Clone)]
pub struct HlsOriginIoContext {
    pub ctx: HlsCtx,
    pub client_addr: SocketAddr,
    pub allow_grace: bool,
    pub priority: i8,
    pub connection_kind: ConnectionKind,
    pub reservation_ttl_secs: u64,
    pub preacquired_provider_handle: Option<Arc<std::sync::Mutex<Option<tuliprox_session::ManagedProviderHandle>>>>,
    pub started_generation: Option<u64>,
}

impl HlsOriginIoContext {
    pub const fn with_grace(mut self, allow_grace: bool) -> Self {
        self.allow_grace = allow_grace;
        self
    }

    pub fn with_preacquired_provider_handle(mut self, provider_handle: ProviderHandle) -> Self {
        let managed =
            tuliprox_session::ManagedProviderHandle::new(Arc::clone(&self.ctx.active_provider), provider_handle);
        self.preacquired_provider_handle = Some(Arc::new(std::sync::Mutex::new(Some(managed))));
        self
    }

    pub fn take_preacquired_provider_handle(&self) -> Option<tuliprox_session::ManagedProviderHandle> {
        let handle = self.preacquired_provider_handle.as_ref()?;
        let mut guard = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.take()
    }
}

pub fn build_hls_origin_session_owner(proxy_session_id: &ProxySessionId) -> String {
    format!("hls-cache:{}", proxy_session_id.0.replace('|', ""))
}

pub fn hls_origin_account_status(ctx: &HlsCtx, binding: &HlsOriginAccountBinding) -> HlsOriginAccountStatus {
    let Some(provider_config) = ctx.active_provider.find_provider_config(&binding.account_name) else {
        return HlsOriginAccountStatus::Missing;
    };
    if !ctx.active_provider.is_provider_for_input(&binding.account_name, &binding.input_name) {
        return HlsOriginAccountStatus::Missing;
    }
    if is_input_expired(provider_config.exp_date()) {
        return HlsOriginAccountStatus::Expired;
    }
    HlsOriginAccountStatus::Known
}

pub fn acquire_bound_hls_origin_account_handle(
    ctx: &HlsCtx,
    binding: &HlsOriginAccountBinding,
    client_addr: &SocketAddr,
    allow_grace: bool,
    priority: i8,
    connection_kind: ConnectionKind,
) -> Result<ProviderHandle, HlsBoundAccountAcquireErrorKind> {
    if binding.is_detached() {
        return Err(HlsBoundAccountAcquireErrorKind::Detached);
    }
    match hls_origin_account_status(ctx, binding) {
        HlsOriginAccountStatus::Missing => return Err(HlsBoundAccountAcquireErrorKind::Missing),
        HlsOriginAccountStatus::Expired => return Err(HlsBoundAccountAcquireErrorKind::Expired),
        HlsOriginAccountStatus::Known => {}
    }
    if ctx.active_provider.is_provider_reserved_for_other_session(&binding.account_name, Some(&binding.session_owner)) {
        return Err(HlsBoundAccountAcquireErrorKind::ReservedForOther);
    }
    let handle = ctx.active_provider.acquire_exact_connection_with_lease_for_session(
        &binding.account_name,
        client_addr,
        allow_grace,
        priority,
        connection_kind,
        Some(PlaybackLeaseRef::new(&binding.session_owner, PlaybackKind::LiveHls)),
    );
    if let Some(handle) = handle {
        return Ok(handle);
    }
    let kind = if ctx.active_provider.is_exhausted(&binding.account_name) {
        HlsBoundAccountAcquireErrorKind::Exhausted
    } else {
        HlsBoundAccountAcquireErrorKind::Unavailable
    };
    debug!(
        "HLS bound origin account unavailable: account={} owner={} reason={kind:?}",
        sanitize_sensitive_info(binding.account_name.as_ref()),
        sanitize_sensitive_info(&binding.session_owner)
    );
    Err(kind)
}

pub async fn begin_hls_origin_account_io(
    origin_io: &HlsOriginIoContext,
    session: &HlsSessionHandle,
    binding: &HlsOriginAccountBinding,
) -> Result<HlsOriginAccountIoLeaseGuard, HlsBoundAccountAcquireErrorKind> {
    begin_hls_origin_account_io_inner(origin_io, session, binding, None).await
}

pub async fn begin_hls_origin_account_io_bounded(
    origin_io: &HlsOriginIoContext,
    session: &HlsSessionHandle,
    binding: &HlsOriginAccountBinding,
    acquire_timeout: Duration,
) -> Result<HlsOriginAccountIoLeaseGuard, HlsBoundAccountAcquireErrorKind> {
    begin_hls_origin_account_io_inner(origin_io, session, binding, Some(acquire_timeout)).await
}

async fn begin_hls_origin_account_io_inner(
    origin_io: &HlsOriginIoContext,
    session: &HlsSessionHandle,
    binding: &HlsOriginAccountBinding,
    acquire_timeout: Option<Duration>,
) -> Result<HlsOriginAccountIoLeaseGuard, HlsBoundAccountAcquireErrorKind> {
    if binding.is_detached() {
        return Err(HlsBoundAccountAcquireErrorKind::Detached);
    }

    let deadline = acquire_timeout.map(|duration| Instant::now() + duration);
    let slot = reserve_hls_origin_account_io_slot(session, binding, deadline).await?;
    if let HlsOriginAccountIoSlot::Joined(lease) = slot {
        let guard = HlsOriginAccountIoLeaseGuard::new(binding.clone(), lease);
        // Release any unused pre-acquired handle synchronously on drop. It needs no
        // cleanup permit, so a join cannot self-block on a single-permit cleanup queue.
        drop(origin_io.take_preacquired_provider_handle());
        return Ok(guard);
    }

    let HlsOriginAccountIoSlot::Acquire(lease) = slot else {
        return Err(HlsBoundAccountAcquireErrorKind::StoreRace);
    };
    // Own the pending lease before the first suspend after the slot reserve: the
    // guard's Drop clears `acquiring` and wakes waiters if the cleanup-permit wait
    // below times out, is cancelled, or the cleanup channel is closed.
    let mut pending_guard = PendingAcquireGuard::new(Arc::clone(&lease));
    let cleanup_permit = reserve_cleanup_permit(origin_io, deadline).await?;

    let acquired_handle = if let Some(mut managed) = origin_io.take_preacquired_provider_handle() {
        // Disarm the managed owner only after the cleanup permit is already reserved,
        // so the naked handle is immediately paired with its release responsibility.
        // The wrapper always carries its handle; fall back to a fresh acquire defensively.
        match managed.disarm() {
            Some(handle) => Ok(handle),
            None => acquire_bound_hls_origin_account_handle(
                &origin_io.ctx,
                binding,
                &origin_io.client_addr,
                origin_io.allow_grace,
                origin_io.priority,
                origin_io.connection_kind,
            ),
        }
    } else {
        acquire_bound_hls_origin_account_handle(
            &origin_io.ctx,
            binding,
            &origin_io.client_addr,
            origin_io.allow_grace,
            origin_io.priority,
            origin_io.connection_kind,
        )
    };

    match acquired_handle {
        Ok(handle) => {
            store_acquired_hls_origin_account_io_handle(
                origin_io,
                session,
                binding,
                handle,
                cleanup_permit,
                pending_guard,
                deadline,
            )
            .await
        }
        Err(err) => {
            clear_pending_hls_origin_account_io_lease(session, &pending_guard.lease, deadline).await;
            pending_guard.disarm();
            Err(err)
        }
    }
}

async fn reserve_cleanup_permit(
    origin_io: &HlsOriginIoContext,
    deadline: Option<Instant>,
) -> Result<tokio::sync::mpsc::OwnedPermit<CleanupEvent>, HlsBoundAccountAcquireErrorKind> {
    let reserve = origin_io.ctx.connection_manager.control_cleanup_tx().reserve_owned();
    match deadline {
        Some(deadline) => timeout(deadline.saturating_duration_since(Instant::now()), reserve)
            .await
            .map_err(|_| HlsBoundAccountAcquireErrorKind::AcquireTimedOut)?
            .map_err(|_| HlsBoundAccountAcquireErrorKind::Unavailable),
        None => reserve.await.map_err(|_| HlsBoundAccountAcquireErrorKind::Unavailable),
    }
}

async fn reserve_hls_origin_account_io_slot(
    session: &HlsSessionHandle,
    binding: &HlsOriginAccountBinding,
    wait_deadline: Option<Instant>,
) -> Result<HlsOriginAccountIoSlot, HlsBoundAccountAcquireErrorKind> {
    loop {
        let reserve = async {
            let mut session = session.write().await;
            try_reserve_hls_origin_account_io_slot(&mut session, binding)
        };
        let outcome = match wait_deadline {
            Some(deadline) => timeout(deadline.saturating_duration_since(Instant::now()), reserve)
                .await
                .map_err(|_| HlsBoundAccountAcquireErrorKind::WaitTimedOut)?,
            None => reserve.await,
        };
        match outcome {
            HlsOriginAccountIoReserveOutcome::Joined(lease) => {
                return Ok(HlsOriginAccountIoSlot::Joined(lease));
            }
            HlsOriginAccountIoReserveOutcome::Acquire(lease) => {
                return Ok(HlsOriginAccountIoSlot::Acquire(lease));
            }
            HlsOriginAccountIoReserveOutcome::Wait(notify) => {
                if let Some(deadline) = wait_deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(HlsBoundAccountAcquireErrorKind::WaitTimedOut);
                    }
                    let wait_for = deadline.saturating_duration_since(now).min(HLS_ORIGIN_ACCOUNT_IO_WAIT_RECHECK);
                    tokio::select! {
                        () = notify.notified() => {}
                        () = tokio::time::sleep(wait_for) => {}
                    }
                } else {
                    tokio::select! {
                        () = notify.notified() => {}
                        () = tokio::time::sleep(HLS_ORIGIN_ACCOUNT_IO_WAIT_RECHECK) => {}
                    }
                }
            }
            HlsOriginAccountIoReserveOutcome::ReservedForOther => {
                return Err(HlsBoundAccountAcquireErrorKind::ReservedForOther);
            }
            HlsOriginAccountIoReserveOutcome::Unavailable => {
                return Err(HlsBoundAccountAcquireErrorKind::Unavailable);
            }
        }
    }
}

enum HlsOriginAccountIoSlot {
    Joined(Arc<HlsOriginAccountIoLease>),
    Acquire(Arc<HlsOriginAccountIoLease>),
}

enum HlsOriginAccountIoReserveOutcome {
    Joined(Arc<HlsOriginAccountIoLease>),
    Acquire(Arc<HlsOriginAccountIoLease>),
    Wait(Arc<Notify>),
    ReservedForOther,
    Unavailable,
}

fn try_reserve_hls_origin_account_io_slot(
    session: &mut HlsSession,
    binding: &HlsOriginAccountBinding,
) -> HlsOriginAccountIoReserveOutcome {
    if session.is_gc_marked_for_removal() {
        return HlsOriginAccountIoReserveOutcome::Unavailable;
    }

    match session.origin_account_io_lease.as_ref() {
        Some(lease) if lease.matches_binding(binding) => {
            if lease.acquiring.load(std::sync::atomic::Ordering::Acquire) {
                HlsOriginAccountIoReserveOutcome::Wait(Arc::clone(&lease.notify))
            } else if lease.try_join() {
                HlsOriginAccountIoReserveOutcome::Joined(Arc::clone(lease))
            } else {
                let lease = Arc::new(HlsOriginAccountIoLease::acquiring(binding));
                session.origin_account_io_lease = Some(Arc::clone(&lease));
                HlsOriginAccountIoReserveOutcome::Acquire(lease)
            }
        }
        Some(lease) if lease.is_active_or_acquiring() => HlsOriginAccountIoReserveOutcome::ReservedForOther,
        _ => {
            let lease = Arc::new(HlsOriginAccountIoLease::acquiring(binding));
            session.origin_account_io_lease = Some(Arc::clone(&lease));
            HlsOriginAccountIoReserveOutcome::Acquire(lease)
        }
    }
}

async fn store_acquired_hls_origin_account_io_handle(
    origin_io: &HlsOriginIoContext,
    session: &HlsSessionHandle,
    binding: &HlsOriginAccountBinding,
    handle: ProviderHandle,
    cleanup_permit: tokio::sync::mpsc::OwnedPermit<CleanupEvent>,
    mut pending_guard: PendingAcquireGuard,
    deadline: Option<Instant>,
) -> Result<HlsOriginAccountIoLeaseGuard, HlsBoundAccountAcquireErrorKind> {
    let playback_request_id = handle.playback_request_id;
    // The handle carries the exact provider binding tag it was acquired under, so a
    // later detach/clear can prove it still targets this binding, not a successor.
    let provider_binding_tag = handle.binding_tag;
    let mut handle_guard = ManagedHandleGuard::new(handle, cleanup_permit);
    if let Some(request_id) = playback_request_id {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            clear_pending_hls_origin_account_io_lease(session, &pending_guard.lease, deadline).await;
            pending_guard.disarm();
            return Err(HlsBoundAccountAcquireErrorKind::AcquireTimedOut);
        }
        let lease_ref = PlaybackLeaseRef { owner: &binding.session_owner, kind: PlaybackKind::LiveHls, request_id };
        origin_io.ctx.active_provider.refresh_playback_lease(
            &binding.account_name,
            &lease_ref,
            origin_io.reservation_ttl_secs,
        );
    }

    let lease = Arc::clone(&pending_guard.lease);

    let store = async {
        let mut session_guard = session.write().await;
        let session_lease_matches =
            session_guard.origin_account_io_lease.as_ref().is_some_and(|current| Arc::ptr_eq(current, &lease));
        let binding_still_current = session_guard.origin_account_binding.as_ref().is_some_and(|current| {
            current.generation == binding.generation
                && current.account_name == binding.account_name
                && current.session_owner == binding.session_owner
                && current.is_active()
        });
        if !session_lease_matches
            || !binding_still_current
            || session_guard.is_gc_marked_for_removal()
            || !matches!(hls_origin_account_status(&origin_io.ctx, binding), HlsOriginAccountStatus::Known)
        {
            return None;
        }

        let (handle, permit) = handle_guard.take_resource()?;
        match lease.provider_handle.lock() {
            Ok(mut guard) => *guard = Some(handle),
            Err(poisoned) => *poisoned.into_inner() = Some(handle),
        }
        match lease.cleanup_permit.lock() {
            Ok(mut guard) => *guard = Some(permit),
            Err(poisoned) => *poisoned.into_inner() = Some(permit),
        }
        lease.active_io_count.store(1, std::sync::atomic::Ordering::Release);
        lease.acquiring.store(false, std::sync::atomic::Ordering::Release);
        lease.notify.notify_waiters();
        if let Some(current) = session_guard.origin_account_binding.as_mut() {
            current.playback_request_id = playback_request_id;
            current.provider_binding_tag = provider_binding_tag;
        }
        let mut guard_binding = binding.clone();
        guard_binding.playback_request_id = playback_request_id;
        guard_binding.provider_binding_tag = provider_binding_tag;
        Some(HlsOriginAccountIoLeaseGuard::new(guard_binding, Arc::clone(&lease)))
    };

    let lease_guard = match deadline {
        Some(deadline) => {
            if let Ok(guard) = timeout(deadline.saturating_duration_since(Instant::now()), store).await {
                guard
            } else {
                clear_pending_hls_origin_account_io_lease(session, &pending_guard.lease, Some(deadline)).await;
                pending_guard.disarm();
                return Err(HlsBoundAccountAcquireErrorKind::AcquireTimedOut);
            }
        }
        None => store.await,
    };

    if let Some(lease_guard) = lease_guard {
        pending_guard.disarm();
        Ok(lease_guard)
    } else {
        clear_pending_hls_origin_account_io_lease(session, &pending_guard.lease, deadline).await;
        pending_guard.disarm();
        Err(HlsBoundAccountAcquireErrorKind::StoreRace)
    }
}

async fn clear_pending_hls_origin_account_io_lease(
    session: &HlsSessionHandle,
    pending_lease: &Arc<HlsOriginAccountIoLease>,
    deadline: Option<Instant>,
) {
    pending_lease.acquiring.store(false, std::sync::atomic::Ordering::Release);
    pending_lease.notify.notify_waiters();
    let clear = async {
        let mut session = session.write().await;
        if session.origin_account_io_lease.as_ref().is_some_and(|lease| Arc::ptr_eq(lease, pending_lease)) {
            session.origin_account_io_lease = None;
        }
    };
    match deadline {
        Some(deadline) => {
            let _ = timeout(deadline.saturating_duration_since(Instant::now()), clear).await;
        }
        None => clear.await,
    }
}

pub async fn finish_hls_origin_account_io(
    origin_io: &HlsOriginIoContext,
    session: &HlsSessionHandle,
    mut guard: HlsOriginAccountIoLeaseGuard,
    refresh_reservation: bool,
) {
    let binding = guard.binding().clone();
    let mut provider_resource_to_release = None;
    let mut should_refresh_reservation = false;
    let confirmed_ttl_secs;
    {
        let mut session = session.write().await;

        let now_ms = chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default();
        confirmed_ttl_secs = session.activity.last_delivered_media_at_ms.map_or(0, |last| {
            last.saturating_add(origin_io.reservation_ttl_secs.saturating_mul(1000))
                .saturating_sub(now_ms)
                .div_ceil(1000)
        });

        // Decrement directly on guard.lease, not session.origin_account_io_lease.
        // The session may have been rebound or replaced between I/O start and finish.
        // Only clear the session reference if it still points to this exact lease instance.
        guard.disarm();
        if guard.lease.active_io_count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            provider_resource_to_release = guard.lease.take_resource();
            if session
                .origin_account_io_lease
                .as_ref()
                .is_some_and(|session_lease| std::sync::Arc::ptr_eq(session_lease, &guard.lease))
            {
                session.origin_account_io_lease = None;
            }
            if refresh_reservation && confirmed_ttl_secs > 0 {
                should_refresh_reservation = true;
            }
        }

        if let Some(current) = session.origin_account_binding.as_mut().filter(|current| {
            current.is_active()
                && current.account_name == binding.account_name
                && current.session_owner == binding.session_owner
                && current.generation == binding.generation
        }) {
            let now_ms = chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default();
            current.last_origin_io_at_ms = Some(now_ms);
            if should_refresh_reservation {
                current.last_reservation_refresh_at_ms = Some(now_ms);
            }
        }
    }

    if let Some((provider_handle, cleanup_permit)) = provider_resource_to_release {
        let mut handle_guard = ManagedHandleGuard::new(provider_handle, cleanup_permit);
        if should_refresh_reservation {
            if let Some(request_id) = binding.playback_request_id {
                origin_io.ctx.active_provider.refresh_playback_lease(
                    &binding.account_name,
                    &PlaybackLeaseRef { owner: &binding.session_owner, kind: PlaybackKind::LiveHls, request_id },
                    confirmed_ttl_secs,
                );
            }
        }
        if let Some((handle, permit)) = handle_guard.take_resource() {
            permit.send(CleanupEvent::ReleaseProviderHandle { handle: Some(handle) });
        }
    }
}

pub fn origin_account_binding_from_allocation(
    input_name: Arc<str>,
    proxy_session_id: &ProxySessionId,
    allocation: &ProviderAllocation,
    now_ms: u64,
) -> Option<HlsOriginAccountBinding> {
    let account_name = allocation.get_provider_name()?;
    Some(HlsOriginAccountBinding::new(input_name, account_name, proxy_session_id, now_ms))
}

#[cfg(test)]
mod tests {
    use super::{
        build_hls_origin_session_owner, classify_account_binding_protection, reserve_hls_origin_account_io_slot,
        HlsAccountBindingProtection, HlsAccountOverlapTiming, HlsBoundAccountAcquireErrorKind, HlsOriginAccountBinding,
        HlsOriginAccountBindingMode, HlsOriginAccountIoLease, HlsOriginAccountIoSlot, HlsOriginAccountRebindState,
        HlsOriginSource, HlsOriginSourceKind,
    };
    use crate::{build_proxy_session_id, HlsSession, HlsSessionKey, ProxySessionId};
    use std::sync::Arc;

    #[tokio::test]
    async fn account_io_wait_releases_the_session_write_lock_before_awaiting_notification() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "lock-release"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease::acquiring(&binding)));
        let session = Arc::new(tokio::sync::RwLock::new(session));
        let waiting_session = Arc::clone(&session);
        let waiting_binding = binding.clone();
        let waiter =
            tokio::spawn(
                async move { reserve_hls_origin_account_io_slot(&waiting_session, &waiting_binding, None).await },
            );
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the acquiring lease must make the second caller wait");

        let completing_session = Arc::clone(&session);
        let completion = tokio::spawn(async move {
            let notify = {
                let mut session = completing_session.write().await;
                session.origin_account_io_lease.take().map(|lease| std::sync::Arc::clone(&lease.notify))
            };
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
        });
        tokio::task::yield_now().await;
        assert!(completion.is_finished(), "the waiter must not retain the session lock");
        completion.await.expect("controlled completion task");
        assert!(matches!(waiter.await.expect("controlled waiter task"), Ok(HlsOriginAccountIoSlot::Acquire(_))));
    }

    #[test]
    fn hls_origin_session_owner_contains_no_reservation_family_separator() {
        let owner = build_hls_origin_session_owner(&ProxySessionId("abc|def".to_string()));

        assert_eq!(owner, "hls-cache:abcdef");
        assert!(!owner.contains('|'));
    }

    #[test]
    fn released_origin_io_lease_cannot_be_rejoined() {
        let session = HlsSession::new(HlsSessionKey::new(1, "released-io"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        let lease = HlsOriginAccountIoLease::active_for_test(&binding, 0);

        assert!(!lease.try_join());
        assert_eq!(lease.active_io_count.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn active_origin_io_lease_cannot_be_replaced_by_another_generation() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "generation-conflict"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        let conflicting = HlsOriginAccountBinding::rebound(
            Arc::clone(&binding.input_name),
            Arc::clone(&binding.account_name),
            binding.session_owner.clone(),
            binding.generation + 1,
            1_001,
        );
        let lease = Arc::new(HlsOriginAccountIoLease::active_for_test(&binding, 1));
        session.origin_account_io_lease = Some(Arc::clone(&lease));

        assert!(matches!(
            super::try_reserve_hls_origin_account_io_slot(&mut session, &conflicting),
            super::HlsOriginAccountIoReserveOutcome::ReservedForOther
        ));
        assert!(session.origin_account_io_lease.as_ref().is_some_and(|current| Arc::ptr_eq(current, &lease)));
        assert_eq!(lease.active_io_count.load(std::sync::atomic::Ordering::Acquire), 1);
    }

    #[test]
    fn gc_marked_session_rejects_origin_io_reservation() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "gc-marked"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.mark_for_gc_removal();

        assert!(matches!(
            super::try_reserve_hls_origin_account_io_slot(&mut session, &binding),
            super::HlsOriginAccountIoReserveOutcome::Unavailable
        ));
        assert!(session.origin_account_io_lease.is_none());
    }

    #[test]
    fn origin_source_session_key_uses_input_and_stream_ref() {
        let direct = HlsOriginSource::new(7, Arc::from("input"), "80510", HlsOriginSourceKind::XtreamLive);

        assert_eq!(direct.session_key().stable_value(), "input:7|hls|80510");
    }

    #[test]
    fn origin_source_preserves_alphanumeric_input_stream_id() {
        let source =
            HlsOriginSource::new(7, Arc::from("input"), "m3u-channel_A42", HlsOriginSourceKind::M3uMediaPlaylist);

        assert_eq!(source.stream_ref, "m3u-channel_A42");
        assert_eq!(source.session_key().stable_value(), "input:7|hls|m3u-channel_A42");
    }

    #[test]
    fn account_overlap_timing_uses_target_duration_or_fallback() {
        let timing = HlsAccountOverlapTiming::from_target_duration_secs(Some(12));
        assert_eq!(timing.target_duration_ms, 12_000);
        assert_eq!(timing.hard_active_window_ms, 12_000);
        assert_eq!(timing.soft_active_window_ms, 24_000);
        assert_eq!(timing.reservation_ttl_secs(), 37);

        let fallback = HlsAccountOverlapTiming::from_target_duration_secs(None);
        assert_eq!(fallback.target_duration_ms, 15_000);
        assert_eq!(fallback.hard_active_window_ms, 15_000);
        assert_eq!(fallback.soft_active_window_ms, 30_000);
        assert_eq!(fallback.reservation_ttl_secs(), 46);
    }

    #[test]
    fn account_binding_protection_classifies_hard_soft_and_expired() {
        let timing = HlsAccountOverlapTiming::from_target_duration_secs(Some(10));

        assert_eq!(
            classify_account_binding_protection(Some(1_000), 5_000, &timing),
            HlsAccountBindingProtection::HardActive { until_ms: 11_000 }
        );
        assert_eq!(
            classify_account_binding_protection(Some(1_000), 20_000, &timing),
            HlsAccountBindingProtection::SoftActive { reclaim_until_ms: 31_000 }
        );
        assert_eq!(
            classify_account_binding_protection(Some(1_000), 32_000, &timing),
            HlsAccountBindingProtection::Expired
        );
        assert_eq!(classify_account_binding_protection(None, 1_000, &timing), HlsAccountBindingProtection::NoMediaYet);
    }

    #[test]
    fn speculative_binding_records_displaced_session_and_can_promote() {
        let proxy_session_id = ProxySessionId("new-session".to_string());
        let displaced = ProxySessionId("old-session".to_string());
        let mut binding = HlsOriginAccountBinding::speculative_from(
            Arc::from("input"),
            Arc::from("account"),
            &proxy_session_id,
            displaced.clone(),
            10_000,
            1_000,
        );

        assert_eq!(
            binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative {
                displaced_proxy_session_id: displaced,
                reclaim_until_ms: 10_000,
            }
        );

        binding.promote_to_active();
        assert_eq!(binding.binding_mode, HlsOriginAccountBindingMode::Active);
    }

    #[test]
    fn provider_failover_mirror_urls_do_not_affect_session_or_proxy_identity() {
        let mirror_a = "http://mirror-a.example.com";
        let mirror_b = "http://mirror-b.example.com";
        let failover = HlsOriginSource::new(7, Arc::from("input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist);
        let direct = HlsOriginSource::new(7, Arc::from("input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist);
        let secret = b"rewrite-secret";

        assert_eq!(failover.session_key(), direct.session_key());
        assert_eq!(
            build_proxy_session_id(&failover.session_key(), secret),
            build_proxy_session_id(&direct.session_key(), secret)
        );
        assert!(!failover.session_key().stable_value().contains(mirror_a));
        assert!(!failover.session_key().stable_value().contains(mirror_b));
        assert!(!failover.session_key().stable_value().contains("provider://"));
    }

    #[test]
    fn origin_account_binding_debug_excludes_origin_and_provider_url_fields() {
        let binding = HlsOriginAccountBinding::new(
            Arc::from("cdn-dev"),
            Arc::from("cdn-dev-alias"),
            &ProxySessionId("proxy-session".to_string()),
            100,
        );
        let debug = format!("{binding:?}");

        assert!(debug.contains("cdn-dev-alias"));
        assert!(!debug.contains("provider://"));
        assert!(!debug.contains("origin_url"));
        assert!(!debug.contains("origin_password"));
        assert!(!debug.contains("mirror"));
        assert!(!debug.contains("proxy-session"));
    }

    #[test]
    fn session_key_fallback_source_uses_key_identity_only() {
        let key = HlsSessionKey::new(9, "stable-content-id");
        let source = HlsOriginSource::from_session_key(&key);

        assert_eq!(source.session_key(), key);
    }

    #[test]
    fn archive_source_keeps_origin_stream_ref_but_uses_distinct_session_key() {
        let live = HlsOriginSource::new(7, Arc::from("input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist);
        let archive = live.clone().with_archive_reference(1_784_898_000);

        assert_eq!(live.stream_ref, "80510");
        assert_eq!(archive.stream_ref, "80510");
        assert_ne!(live.session_key(), archive.session_key());
        assert_eq!(archive.session_key().archive_reference, Some(1_784_898_000));
    }

    #[test]
    fn archive_source_distinguishes_requests_with_same_start() {
        let live = HlsOriginSource::new(7, Arc::from("input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist);
        let short = live.clone().with_archive_request(1_784_898_000, "http://origin/80510-1784898000-1800.m3u8");
        let long = live.with_archive_request(1_784_898_000, "http://origin/80510-1784898000-3600.m3u8");

        assert_ne!(short.session_key(), long.session_key());
    }

    #[test]
    fn bound_account_acquire_error_rebind_policy_matches_concept() {
        assert!(HlsBoundAccountAcquireErrorKind::Missing.allows_rebind());
        assert!(HlsBoundAccountAcquireErrorKind::Expired.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::Exhausted.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::ReservedForOther.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::Detached.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::WaitTimedOut.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::AcquireTimedOut.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::StoreRace.allows_rebind());
        assert!(!HlsBoundAccountAcquireErrorKind::Unavailable.allows_rebind());
    }

    #[test]
    fn bound_account_acquire_error_resource_retry_policy_matches_handoff_semantics() {
        assert!(!HlsBoundAccountAcquireErrorKind::Missing.is_retryable_resource_failure());
        assert!(!HlsBoundAccountAcquireErrorKind::Expired.is_retryable_resource_failure());
        assert!(!HlsBoundAccountAcquireErrorKind::ReservedForOther.is_retryable_resource_failure());
        assert!(!HlsBoundAccountAcquireErrorKind::Detached.is_retryable_resource_failure());
        assert!(HlsBoundAccountAcquireErrorKind::Exhausted.is_retryable_resource_failure());
        assert!(HlsBoundAccountAcquireErrorKind::WaitTimedOut.is_retryable_resource_failure());
        assert!(HlsBoundAccountAcquireErrorKind::AcquireTimedOut.is_retryable_resource_failure());
        assert!(HlsBoundAccountAcquireErrorKind::StoreRace.is_retryable_resource_failure());
        assert!(HlsBoundAccountAcquireErrorKind::Unavailable.is_retryable_resource_failure());
    }

    #[test]
    fn rebound_binding_keeps_owner_and_increments_generation() {
        let original = HlsOriginAccountBinding::new(
            Arc::from("cdn-dev"),
            Arc::from("old-account"),
            &ProxySessionId("proxy-session".to_string()),
            100,
        );

        let rebound = HlsOriginAccountBinding::rebound(
            Arc::clone(&original.input_name),
            Arc::from("new-account"),
            original.session_owner.clone(),
            original.generation.saturating_add(1),
            200,
        );

        assert_eq!(rebound.session_owner, original.session_owner);
        assert_eq!(rebound.generation, original.generation + 1);
        assert_eq!(rebound.account_name.as_ref(), "new-account");
        assert_eq!(rebound.pinned_at_ms, 200);
        assert_eq!(rebound.last_origin_io_at_ms, None);
        assert_eq!(rebound.last_reservation_refresh_at_ms, None);
    }

    #[test]
    fn origin_account_rebind_state_enforces_backoff() {
        let mut state = HlsOriginAccountRebindState::default();

        assert!(state.is_allowed_now(1_000));
        state.mark_attempt_started(Arc::from("old-account"), 1_000);
        assert!(!state.is_allowed_now(1_001));
        state.mark_failed(1_000);

        assert!(!state.is_allowed_now(2_999));
        assert!(state.is_allowed_now(3_000));
        assert_eq!(state.consecutive_rebind_failures, 1);

        state.mark_success();
        assert!(state.is_allowed_now(3_001));
        assert_eq!(state.consecutive_rebind_failures, 0);
        assert_eq!(state.next_rebind_allowed_at_ms, None);
    }

    #[tokio::test]
    async fn origin_account_io_lease_guard_drop_decrements_count_and_cleans_up() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "guard-drop"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease {
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            provider_handle: std::sync::Mutex::new(None),
            cleanup_permit: std::sync::Mutex::new(None),
            acquiring: std::sync::atomic::AtomicBool::new(false),
            active_io_count: std::sync::atomic::AtomicUsize::new(2),
            notify: Arc::new(tokio::sync::Notify::new()),
            generation: 0,
        }));
        let session = Arc::new(tokio::sync::RwLock::new(session));
        let guard = super::HlsOriginAccountIoLeaseGuard::new(
            binding.clone(),
            Arc::clone(session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap()),
        );
        drop(guard);

        let active_count = session
            .read()
            .await
            .origin_account_io_lease
            .as_ref()
            .map(|l| l.active_io_count.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(active_count, Some(1), "drop must decrement active_io_count");
    }

    #[tokio::test]
    async fn pending_acquire_guard_drop_clears_lease_and_notifies() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "pending-drop"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease::acquiring(&binding)));
        let session = Arc::new(tokio::sync::RwLock::new(session));
        let guard = super::PendingAcquireGuard::new(Arc::clone(
            session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap(),
        ));
        drop(guard);

        assert!(
            !session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .unwrap()
                .acquiring
                .load(std::sync::atomic::Ordering::Acquire),
            "pending acquire drop must clear acquiring flag"
        );
    }

    #[tokio::test]
    async fn origin_account_io_lease_guard_drop_under_lock_contention_decrements_and_cleans_up() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "guard-contention"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease {
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            provider_handle: std::sync::Mutex::new(None),
            cleanup_permit: std::sync::Mutex::new(None),
            acquiring: std::sync::atomic::AtomicBool::new(false),
            active_io_count: std::sync::atomic::AtomicUsize::new(2),
            notify: Arc::new(tokio::sync::Notify::new()),
            generation: 0,
        }));
        let session = Arc::new(tokio::sync::RwLock::new(session));
        let guard = super::HlsOriginAccountIoLeaseGuard::new(
            binding.clone(),
            Arc::clone(session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap()),
        );

        let lock = session.read().await;
        drop(guard);
        assert_eq!(
            lock.origin_account_io_lease.as_ref().map(|l| l.active_io_count.load(std::sync::atomic::Ordering::Acquire)),
            Some(1),
            "drop MUST synchronously modify active_io_count even when session is locked"
        );
        drop(lock);

        for _ in 0..100 {
            if session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .map(|l| l.active_io_count.load(std::sync::atomic::Ordering::Acquire))
                == Some(1)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let active_count = session
            .read()
            .await
            .origin_account_io_lease
            .as_ref()
            .map(|l| l.active_io_count.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(active_count, Some(1), "deferred cleanup must decrement active_io_count");
    }

    #[tokio::test]
    async fn pending_acquire_guard_drop_under_lock_contention_clears_lease_and_notifies() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "pending-contention"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        let notify = Arc::new(tokio::sync::Notify::new());
        session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease {
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            provider_handle: std::sync::Mutex::new(None),
            cleanup_permit: std::sync::Mutex::new(None),
            acquiring: std::sync::atomic::AtomicBool::new(true),
            active_io_count: std::sync::atomic::AtomicUsize::new(0),
            notify: Arc::clone(&notify),
            generation: 0,
        }));
        let session = Arc::new(tokio::sync::RwLock::new(session));
        let guard = super::PendingAcquireGuard::new(Arc::clone(
            session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap(),
        ));

        let lock = session.read().await;
        let notified_future = notify.notified();
        drop(guard);
        assert!(
            !lock.origin_account_io_lease.as_ref().unwrap().acquiring.load(std::sync::atomic::Ordering::Acquire),
            "drop MUST synchronously clear acquiring flag"
        );
        drop(lock);

        let notified = tokio::time::timeout(std::time::Duration::from_secs(1), notified_future).await;
        assert!(notified.is_ok(), "deferred cleanup must notify waiters");
        assert!(
            !session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .unwrap()
                .acquiring
                .load(std::sync::atomic::Ordering::Acquire),
            "synchronous drop must clear acquiring flag"
        );
    }

    /// The begin path must own the pending lease before the first cleanup-permit wait:
    /// when the cleanup channel is saturated and the permit wait times out, the pending
    /// lease is cleared instead of staying `acquiring` forever.
    #[tokio::test]
    async fn begin_cleanup_permit_timeout_allows_next_acquire() {
        let ctx = crate::hls_ctx::HlsCtx::for_test(tuliprox_core::model::Config::default());
        let origin_io = super::HlsOriginIoContext {
            ctx: ctx.clone(),
            client_addr: "127.0.0.1:8080".parse().unwrap(),
            allow_grace: false,
            priority: 0,
            connection_kind: tuliprox_session::ConnectionKind::Normal,
            reservation_ttl_secs: 60,
            preacquired_provider_handle: None,
            started_generation: None,
        };

        let mut session = HlsSession::new(HlsSessionKey::new(1, "permit-timeout"), b"secret", 1_000);
        let binding =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
        session.origin_account_binding = Some(binding.clone());
        let session = Arc::new(tokio::sync::RwLock::new(session));

        // Saturate the control cleanup lane so `reserve_cleanup_permit` blocks until
        // the bounded deadline fires.
        let mut held_permits = Vec::new();
        while let Ok(permit) = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            ctx.connection_manager.control_cleanup_tx().reserve_owned(),
        )
        .await
        {
            held_permits.push(permit);
        }

        let result = super::begin_hls_origin_account_io_bounded(
            &origin_io,
            &session,
            &binding,
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(
            matches!(result, Err(HlsBoundAccountAcquireErrorKind::AcquireTimedOut)),
            "a saturated cleanup queue must time out the permit wait, got {result:?}"
        );

        let pending_cleared = session
            .read()
            .await
            .origin_account_io_lease
            .as_ref()
            .is_none_or(|lease| !lease.acquiring.load(std::sync::atomic::Ordering::Acquire));
        assert!(pending_cleared, "the pending lease must not stay acquiring after the permit timeout");
    }
}
