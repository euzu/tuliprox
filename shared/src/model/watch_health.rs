use serde::{Deserialize, Serialize};

/// Why a target's `watch` config is not doing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchDisabledReason {
    /// The target configured `watch` patterns and none of them compiled, so
    /// the feature turned itself off. One `warn!` at config load was the only
    /// trace.
    InvalidPatterns,
    /// The target carries the reserved default name, which has no stable key
    /// to store watch state under.
    UnnamedTarget,
    /// The watch state file could not be read or written. The group either
    /// silently re-baselines - losing the diff - or stops being tracked.
    StorageFailure,
}

impl WatchDisabledReason {
    /// Stable wire name, for a plugin matching on the reason.
    #[must_use]
    pub const fn as_wire_name(self) -> &'static str {
        match self {
            Self::InvalidPatterns => "invalid_patterns",
            Self::UnnamedTarget => "unnamed_target",
            Self::StorageFailure => "storage_failure",
        }
    }
}

/// A target whose `watch` config is configured but not working.
///
/// An operator sets `watch`, sees nothing for weeks, and has no signal that
/// anything is wrong: every path that disables the feature logs once and
/// carries on. This is that signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchDisabled {
    pub target: String,
    /// The group this concerns, when the failure is per-group rather than
    /// per-target. Only [`WatchDisabledReason::StorageFailure`] sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub reason: WatchDisabledReason,
    /// The underlying error, where there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl WatchDisabled {
    #[must_use]
    pub fn new(target: String, reason: WatchDisabledReason) -> Self {
        Self { target, group: None, reason, detail: None }
    }

    #[must_use]
    pub fn with_group(mut self, group: String) -> Self {
        self.group = Some(group);
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: String) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// `watch` patterns that matched no group in the refreshed playlist.
///
/// The same argument `EventKindMask::from_wire_names` already makes about
/// subscription names: a typo must surface, not silently narrow what is
/// being watched. A pattern that matches nothing is indistinguishable, from
/// the outside, from a pattern whose group has not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchUnmatched {
    pub target: String,
    /// The configured patterns that matched no group this refresh.
    pub patterns: Vec<String>,
    /// How many groups the target produced, so "matched nothing" can be told
    /// apart from "the target produced nothing".
    pub groups_seen: usize,
}

impl WatchUnmatched {
    #[must_use]
    pub fn new(target: String, patterns: Vec<String>, groups_seen: usize) -> Self {
        Self { target, patterns, groups_seen }
    }
}
