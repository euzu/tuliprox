//! The Stalker portal calls this client makes, as a closed set.
//!
//! Actions used to be `&'static str` threaded from the call site into
//! `send_json`, into the body-cap lookup, and into six error variants as a `String`.
//! The cap lookup matched on those strings with a silent fallback, and the strings it
//! matched were not the strings the call sites passed: catalog fetches announced
//! themselves as `get_ordered_list` and `get_all_channels` while the lookup tested for
//! `ordered_list` and `all_channels`. Both fell through to the fallback, so a user who
//! raised `ordered_list_mb` got the default anyway and never found out.
//!
//! An enum cannot have that bug. Every action names its cap, the compiler checks the
//! mapping is total, and the error variants carry something comparable instead of a
//! `String` that call sites had to spell identically to be matched later.

use crate::stalker::client::StalkerBodyCaps;
use std::fmt;

/// Cap for actions whose responses are small and fixed-shape — handshakes, profile,
/// capabilities. Unchanged from the fallback these previously landed on.
pub const DEFAULT_ACTION_BYTES: u64 = 8 * 1024 * 1024;

/// One call against a Stalker portal.
///
/// This is the label an action is known by in caps, errors and capability snapshots. It
/// is deliberately not the `action=` query parameter: two of these (`Handshake` and
/// `HandshakePortal`) send the same query against different endpoints, and telling them
/// apart in an error is the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StalkerAction {
    Handshake,
    HandshakeExtra,
    HandshakePortal,
    DoAuth,
    GetProfile,
    GetCapabilities,
    GetGenres,
    GetCategories,
    GetOrderedList,
    GetAllChannels,
    SeriesInfo,
    CreateLink,
    GetShortEpg,
    GetEpg,
    GetBulkEpg,
}

impl StalkerAction {
    /// The stable name for this action, as it appears in logs, errors and persisted
    /// capability snapshots.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::HandshakeExtra => "handshake-extra",
            Self::HandshakePortal => "handshake-portal",
            Self::DoAuth => "do_auth",
            Self::GetProfile => "get_profile",
            Self::GetCapabilities => "get_capabilities",
            Self::GetGenres => "get_genres",
            Self::GetCategories => "get_categories",
            Self::GetOrderedList => "get_ordered_list",
            Self::GetAllChannels => "get_all_channels",
            Self::SeriesInfo => "series_info",
            Self::CreateLink => "create_link",
            Self::GetShortEpg => "get_short_epg",
            Self::GetEpg => "get_epg",
            Self::GetBulkEpg => "get_epg_bulk",
        }
    }

    /// The response body cap for this action.
    ///
    /// Exhaustive by construction: adding an action without deciding what it may return
    /// does not compile, which is what the old string match could not enforce.
    #[must_use]
    pub const fn cap_bytes(self, caps: &StalkerBodyCaps) -> u64 {
        match self {
            Self::CreateLink => caps.create_link_bytes,
            // Catalog responses. `GetOrderedList` and `GetAllChannels` belong here and
            // silently did not before.
            Self::GetOrderedList
            | Self::GetAllChannels
            | Self::SeriesInfo
            | Self::GetGenres
            | Self::GetCategories => caps.ordered_list_bytes,
            Self::GetShortEpg | Self::GetEpg | Self::GetBulkEpg => caps.get_epg_bytes,
            Self::Handshake
            | Self::HandshakeExtra
            | Self::HandshakePortal
            | Self::DoAuth
            | Self::GetProfile
            | Self::GetCapabilities => DEFAULT_ACTION_BYTES,
        }
    }

    /// Whether a catalog fetch of this action can fall back to another strategy when the
    /// portal does not implement it.
    #[must_use]
    pub const fn is_optional_catalog_shortcut(self) -> bool {
        matches!(self, Self::GetAllChannels)
    }
}

impl fmt::Display for StalkerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{StalkerAction, DEFAULT_ACTION_BYTES};
    use crate::stalker::client::StalkerBodyCaps;

    const ALL: [StalkerAction; 15] = [
        StalkerAction::Handshake,
        StalkerAction::HandshakeExtra,
        StalkerAction::HandshakePortal,
        StalkerAction::DoAuth,
        StalkerAction::GetProfile,
        StalkerAction::GetCapabilities,
        StalkerAction::GetGenres,
        StalkerAction::GetCategories,
        StalkerAction::GetOrderedList,
        StalkerAction::GetAllChannels,
        StalkerAction::SeriesInfo,
        StalkerAction::CreateLink,
        StalkerAction::GetShortEpg,
        StalkerAction::GetEpg,
        StalkerAction::GetBulkEpg,
    ];

    fn distinct_caps() -> StalkerBodyCaps {
        StalkerBodyCaps { create_link_bytes: 111, ordered_list_bytes: 222, get_epg_bytes: 333 }
    }

    /// The defect the enum exists to prevent: a catalog fetch that quietly ignored the
    /// configured `ordered_list` cap because the label it announced and the label the
    /// lookup tested for were different strings.
    #[test]
    fn catalog_actions_use_the_catalog_cap() {
        let caps = distinct_caps();
        for action in [
            StalkerAction::GetOrderedList,
            StalkerAction::GetAllChannels,
            StalkerAction::SeriesInfo,
            StalkerAction::GetGenres,
            StalkerAction::GetCategories,
        ] {
            assert_eq!(action.cap_bytes(&caps), 222, "{action} must honour the configured catalog cap");
        }
    }

    #[test]
    fn playback_and_epg_actions_use_their_own_caps() {
        let caps = distinct_caps();
        assert_eq!(StalkerAction::CreateLink.cap_bytes(&caps), 111);
        for action in [StalkerAction::GetShortEpg, StalkerAction::GetEpg, StalkerAction::GetBulkEpg] {
            assert_eq!(action.cap_bytes(&caps), 333, "{action} must honour the configured EPG cap");
        }
    }

    #[test]
    fn session_actions_keep_the_fixed_default() {
        let caps = distinct_caps();
        for action in [
            StalkerAction::Handshake,
            StalkerAction::HandshakeExtra,
            StalkerAction::HandshakePortal,
            StalkerAction::DoAuth,
            StalkerAction::GetProfile,
            StalkerAction::GetCapabilities,
        ] {
            assert_eq!(action.cap_bytes(&caps), DEFAULT_ACTION_BYTES);
        }
    }

    #[test]
    fn every_action_has_a_distinct_name() {
        let mut names: Vec<&str> = ALL.iter().map(|action| action.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two actions share a label and would be indistinguishable in errors");
    }

    /// The two handshake calls send the same `action=` query against different endpoints;
    /// an error must still say which one failed.
    #[test]
    fn the_two_handshake_calls_are_distinguishable() {
        assert_ne!(StalkerAction::Handshake.as_str(), StalkerAction::HandshakePortal.as_str());
    }
}
