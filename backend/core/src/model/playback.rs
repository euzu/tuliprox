//! Request, provider-lease and shared-subscriber identities.

use shared::{
    defaults::{DASH_EXT, HLS_EXT},
    model::PlaylistItemType,
};
use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);

/// Identity of one shared-stream subscriber, allocated from the user-stream UID sequence.
///
/// A shared stream fans one provider connection out to several clients. Keying those
/// subscribers by socket address collapses two external clients that arrive through
/// one reverse-proxy socket into a single entry, so the second subscription replaces
/// and cancels the first. Subscribers therefore own a unique id and the socket stays
/// display metadata plus an explicit socket-wide close target.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SharedSubscriberId(u32);

impl SharedSubscriberId {
    #[inline]
    pub const fn from_stream_uid(value: u32) -> Self { Self(value) }

    #[inline]
    pub const fn stream_uid(self) -> u32 { self.0 }
}

impl fmt::Display for SharedSubscriberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "sub#{}", self.0) }
}

/// Server-side unique number of one playback request.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaybackRequestId(u64);

impl PlaybackRequestId {
    #[inline]
    pub fn next() -> Self { Self(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)) }

    /// Constructs a caller-supplied id for tests or diagnostics.
    #[inline]
    pub const fn from_raw(value: u64) -> Self { Self(value) }

    #[inline]
    pub const fn as_u64(self) -> u64 { self.0 }
}

impl fmt::Display for PlaybackRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "req#{}", self.0) }
}

/// Identifier of one logical provider capacity slot lease.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaybackLeaseId(u64);

impl PlaybackLeaseId {
    #[inline]
    pub fn next() -> Self { Self(NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed)) }

    #[inline]
    pub const fn from_raw(value: u64) -> Self { Self(value) }

    #[inline]
    pub const fn as_u64(self) -> u64 { self.0 }
}

impl fmt::Display for PlaybackLeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "lease#{}", self.0) }
}

/// Unambiguous identity of one provider binding incarnation.
///
/// `lease_id` alone survives remove/recreate, while `generation` distinguishes a
/// same-account rebind that reuses the same lease. Together they let a delayed
/// detach/clear prove it still targets the exact binding it originally acquired,
/// instead of the current owner row (which may already be a successor).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ProviderBindingTag {
    pub lease_id: PlaybackLeaseId,
    pub generation: u64,
}

impl ProviderBindingTag {
    #[inline]
    pub const fn new(lease_id: PlaybackLeaseId, generation: u64) -> Self { Self { lease_id, generation } }
}

/// Media class of a playback. Part of the family key, so a live TS playback and a
/// catchup playback of the same channel never share a lease.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PlaybackKind {
    LiveTs,
    LiveHls,
    LiveDash,
    Vod,
    Series,
    Catchup,
    /// Background work (probe, metadata, download, recording). Never reconnect-capable.
    Internal,
}

impl PlaybackKind {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LiveTs => "live_ts",
            Self::LiveHls => "live_hls",
            Self::LiveDash => "live_dash",
            Self::Vod => "vod",
            Self::Series => "series",
            Self::Catchup => "catchup",
            Self::Internal => "internal",
        }
    }

    /// True when an interrupted playback of this kind may keep its provider slot
    /// for a short reconnect window instead of releasing it immediately.
    #[inline]
    pub const fn is_reconnect_capable(self) -> bool {
        matches!(self, Self::LiveHls | Self::LiveDash | Self::Vod | Self::Series | Self::Catchup)
    }

    /// Classifies a playlist item plus its effective playback extension.
    ///
    /// The extension matters: a live item served as `.m3u8` is an adaptive playback
    /// whose reconnects arrive as separate requests, while the same item served as
    /// `.ts` is one long response.
    pub fn classify(item_type: PlaylistItemType, extension: Option<&str>) -> Self {
        let is_hls = extension.is_some_and(|ext| ext.eq_ignore_ascii_case(HLS_EXT));
        let is_dash = extension.is_some_and(|ext| ext.eq_ignore_ascii_case(DASH_EXT));
        match item_type {
            PlaylistItemType::Catchup => Self::Catchup,
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => Self::Vod,
            PlaylistItemType::Series
            | PlaylistItemType::SeriesInfo
            | PlaylistItemType::LocalSeries
            | PlaylistItemType::LocalSeriesInfo => Self::Series,
            PlaylistItemType::LiveDash => Self::LiveDash,
            PlaylistItemType::LiveHls => Self::LiveHls,
            PlaylistItemType::Live | PlaylistItemType::LiveUnknown => {
                if is_dash {
                    Self::LiveDash
                } else if is_hls {
                    Self::LiveHls
                } else {
                    Self::LiveTs
                }
            }
        }
    }
}

impl fmt::Display for PlaybackKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}

/// Why a playback request ended. Each outcome has an explicit lease policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackRequestOutcome {
    Completed,
    ClientClosed,
    FailedBeforeMedia,
    ProviderFailed,
    Preempted,
    Kicked,
    TimedOut,
    SessionExpired,
    ServerShutdown,
}

impl PlaybackRequestOutcome {
    /// True when the lease may stay as a reconnect-capable idle lease.
    ///
    /// Only a clean end of a playback that already produced media keeps the slot;
    /// failures and forced terminations release capacity immediately.
    #[inline]
    pub const fn keeps_idle_lease(self) -> bool { matches!(self, Self::Completed | Self::ClientClosed) }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ClientClosed => "client_closed",
            Self::FailedBeforeMedia => "failed_before_media",
            Self::ProviderFailed => "provider_failed",
            Self::Preempted => "preempted",
            Self::Kicked => "kicked",
            Self::TimedOut => "timed_out",
            Self::SessionExpired => "session_expired",
            Self::ServerShutdown => "server_shutdown",
        }
    }
}

impl fmt::Display for PlaybackRequestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}

/// Explains a provider selection or rejection decision in logs and metrics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackSelectionReason {
    NewPriorityAllocation,
    SameLeaseReattach,
    SharedOriginReuse,
    HigherPriorityUnavailable,
    ReservedCapacity,
    Grace,
    Preemption,
    LeaseExpired,
    StartupAbandoned,
}

impl PlaybackSelectionReason {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewPriorityAllocation => "new-priority-allocation",
            Self::SameLeaseReattach => "same-lease-reattach",
            Self::SharedOriginReuse => "shared-origin-reuse",
            Self::HigherPriorityUnavailable => "higher-priority-unavailable",
            Self::ReservedCapacity => "reserved-capacity",
            Self::Grace => "grace",
            Self::Preemption => "preemption",
            Self::LeaseExpired => "lease-expired",
            Self::StartupAbandoned => "startup-abandoned",
        }
    }
}

impl fmt::Display for PlaybackSelectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_lease_ids_are_unique() {
        let first = PlaybackRequestId::next();
        let second = PlaybackRequestId::next();
        assert_ne!(first, second);
        assert_ne!(PlaybackLeaseId::next(), PlaybackLeaseId::next());
    }

    #[test]
    fn live_extension_selects_adaptive_kind() {
        assert_eq!(PlaybackKind::classify(PlaylistItemType::Live, Some(".ts")), PlaybackKind::LiveTs);
        assert_eq!(PlaybackKind::classify(PlaylistItemType::Live, Some(".m3u8")), PlaybackKind::LiveHls);
        assert_eq!(PlaybackKind::classify(PlaylistItemType::LiveUnknown, Some(".mpd")), PlaybackKind::LiveDash);
        assert_eq!(PlaybackKind::classify(PlaylistItemType::Catchup, None), PlaybackKind::Catchup);
        assert_eq!(PlaybackKind::classify(PlaylistItemType::Video, None), PlaybackKind::Vod);
        assert_eq!(PlaybackKind::classify(PlaylistItemType::Series, None), PlaybackKind::Series);
    }

    #[test]
    fn only_clean_outcomes_keep_idle_leases() {
        assert!(PlaybackRequestOutcome::Completed.keeps_idle_lease());
        assert!(PlaybackRequestOutcome::ClientClosed.keeps_idle_lease());
        assert!(!PlaybackRequestOutcome::FailedBeforeMedia.keeps_idle_lease());
        assert!(!PlaybackRequestOutcome::Preempted.keeps_idle_lease());
        assert!(!PlaybackRequestOutcome::TimedOut.keeps_idle_lease());
        assert!(!PlaybackRequestOutcome::SessionExpired.keeps_idle_lease());
        assert!(!PlaybackRequestOutcome::ServerShutdown.keeps_idle_lease());
    }

    #[test]
    fn live_ts_is_not_reconnect_capable() {
        assert!(!PlaybackKind::LiveTs.is_reconnect_capable());
        assert!(PlaybackKind::LiveHls.is_reconnect_capable());
        assert!(!PlaybackKind::Internal.is_reconnect_capable());
    }
}
