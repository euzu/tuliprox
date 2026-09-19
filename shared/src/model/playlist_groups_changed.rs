use serde::{Deserialize, Serialize};

/// Groups that appeared in or vanished from a target between refreshes.
///
/// The `watch` feature tracks channels *inside* named groups, and is blind to
/// the group set itself in both directions. A group appearing is silent:
/// `process_group_watch` finds no baseline file, writes one, and emits
/// nothing, so the group's entire channel list reads as "not new" from then
/// on. A group vanishing is worse - it is absent from the refreshed playlist,
/// so nothing iterates it and no code path observes the disappearance at all.
///
/// Sampled the same way [`WatchChanges`](crate::model::WatchChanges) is: the
/// lists carry group titles only, and the counts stay true whatever the lists
/// hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistGroupsChanged {
    pub target: String,
    /// Group titles that were not in the previous refresh.
    pub added: Vec<String>,
    /// Group titles that were in the previous refresh and are gone now.
    pub removed: Vec<String>,
    #[serde(default)]
    pub added_total: usize,
    #[serde(default)]
    pub removed_total: usize,
    /// Whether the lists are a sample rather than the whole change.
    #[serde(default)]
    pub truncated: bool,
}

impl PlaylistGroupsChanged {
    /// A complete change set, nothing sampled.
    #[must_use]
    pub fn new(target: String, added: Vec<String>, removed: Vec<String>) -> Self {
        Self { target, added_total: added.len(), removed_total: removed.len(), added, removed, truncated: false }
    }
}
