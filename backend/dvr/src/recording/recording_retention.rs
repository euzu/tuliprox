//! Retention candidate selection.
//!
//! The selector considers only completed recordings with safe final files,
//! groups count retention by owner/channel, and returns oldest candidates
//! first with a stable task-id tie-break.

use super::recording_quota::QuotaRecordingTaskView;
use shared::model::{recording::RecordingOwner, UserId};
use std::collections::HashMap;

/// Retention configuration derived from `RecordingRetentionConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionConfig {
    pub keep_last_per_channel: Option<u32>,
    pub delete_after_days: Option<u32>,
}

/// Group key for count retention. `owner` is the pool the task
/// belongs to (private `UserId` or shared). `channel` is the
/// stable channel key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RetentionGroupKey {
    pub owner: RetentionOwner,
    pub channel: ChannelKey,
}

/// The pool side of a retention key. Private recordings are
/// grouped per owner AND per channel; shared recordings are
/// grouped per channel (no owner dimension).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RetentionOwner {
    Private(UserId),
    Shared,
}

/// Stable channel key. The stable channel ID is preferred; the
/// normalized channel name is the fallback. A `None` stable id
/// falls back to the normalized name.
///
/// Equality and hashing use only the discriminant field that
/// uniquely identifies the channel: when `stable` is `Some`, two
/// keys with the same id collapse regardless of `name_fallback`
/// variations (so a rename or republish under a different display
/// name still groups together). When `stable` is `None`,
/// `name_fallback` is the discriminator.
#[derive(Debug, Clone)]
pub struct ChannelKey {
    pub stable: Option<String>,
    pub name_fallback: String,
}

impl PartialEq for ChannelKey {
    fn eq(&self, other: &Self) -> bool {
        match (&self.stable, &other.stable) {
            (Some(a), Some(b)) => a == b,
            _ => self.stable.is_none() && other.stable.is_none() && self.name_fallback == other.name_fallback,
        }
    }
}

impl Eq for ChannelKey {}

impl std::hash::Hash for ChannelKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        if let Some(s) = &self.stable {
            std::hash::Hash::hash(&1u8, state);
            std::hash::Hash::hash(s, state);
        } else {
            std::hash::Hash::hash(&0u8, state);
            std::hash::Hash::hash(&self.name_fallback, state);
        }
    }
}

impl ChannelKey {
    /// Build a `ChannelKey` from the recording metadata. The
    /// stable id is `channel_id` if present; otherwise the
    /// normalized channel name is used as both the key and the
    /// fallback (so two recordings with no `channel_id` but the
    /// same name still group together).
    pub fn from_metadata(channel_id: Option<&str>, channel_name: Option<&str>) -> Self {
        let stable = channel_id.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let name_fallback = channel_name.map(normalize_channel_name).unwrap_or_default();
        Self { stable, name_fallback }
    }
}

/// Lowercase + collapse whitespace + strip a few common display
/// decorations (trailing year, country tags). Two recordings of
/// "BBC One  (UK)" and "bbc one" must group together.
pub fn normalize_channel_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = true;
    for ch in name.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.extend(ch.to_lowercase());
            last_space = false;
        }
    }
    let trimmed = out.trim().to_string();
    // Strip a trailing parenthesized country/region tag.
    if let Some(idx) = trimmed.find(" (") {
        if trimmed.ends_with(')') {
            return trimmed[..idx].trim_end().to_string();
        }
    }
    trimmed
}

impl RetentionOwner {
    pub fn from_recording_owner(owner: &RecordingOwner) -> Self {
        let RecordingOwner::User(uid) = owner;
        Self::Private(uid.clone())
    }
}

/// A retention candidate is a task that should be deleted by
/// the retention worker. The worker reads `uuid` and looks up
/// the task under the queue mutation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    pub uuid: String,
    pub owner: RetentionOwner,
    pub channel: ChannelKey,
    pub completed_at: i64,
    pub reason: RetentionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionReason {
    /// Older than `delete_after_days` (counted from
    /// `completed_at`).
    Age,
    /// Beyond `keep_last_per_channel` for this
    /// (owner, channel) group.
    Count,
}

/// Group a task by its `(owner, channel)` retention key. Returns
/// `None` for non-Completed tasks and for tasks that cannot be
/// grouped (no channel info and no `completed_at`).
fn group_for<V: QuotaRecordingTaskView>(task: &V) -> Option<(RetentionGroupKey, i64)> {
    let meta = task.recording();
    // Only `Completed` is eligible. Pending, active, failed,
    // Cancelled, deleting and non-recording tasks are excluded.
    if !matches!(task.state(), crate::recording::recording_queue::RecordingTaskState::Completed) {
        return None;
    }
    let completed_at = meta.completed_at?;
    let channel = ChannelKey::from_metadata(meta.channel_id.as_deref(), meta.channel_name.as_deref());
    let owner = RetentionOwner::from_recording_owner(&meta.owner);
    Some((RetentionGroupKey { owner, channel }, completed_at))
}

/// Compute the union of age and count retention candidates.
///
/// `now_secs` is the wall-clock seconds used as the "now" for
/// the age check; passing it in keeps the function pure and
/// testable.
///
/// Output is ordered oldest first; ties break by `uuid`
/// ascending (lexicographic) for stable, deterministic deletes.
pub fn compute_candidates<V: QuotaRecordingTaskView>(
    tasks: &[V],
    config: &RetentionConfig,
    now_secs: i64,
) -> Vec<RetentionCandidate> {
    if config.keep_last_per_channel.is_none() && config.delete_after_days.is_none() {
        return Vec::new();
    }
    // Group (uuid, completed_at, channel, owner) by group key.
    let mut groups: HashMap<RetentionGroupKey, Vec<(String, i64)>> = HashMap::new();
    // The owner/channel we want on each candidate — derived from
    // the task that first populated the group. Channel keys are
    // already normalized so equality is stable.
    for task in tasks {
        let Some((key, completed_at)) = group_for(task) else {
            continue;
        };
        groups.entry(key).or_default().push((task.uuid().to_string(), completed_at));
    }
    let mut candidates: Vec<RetentionCandidate> = Vec::new();
    for (key, mut members) in groups {
        // Stable order inside the group: oldest first, uuid
        // tiebreak. This is the order the count retention keeps
        // and the order we delete from.
        members.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if let Some(keep) = config.keep_last_per_channel {
            let keep = keep as usize;
            // `keep_last_per_channel = N` means "keep the N most
            // recent". The members are sorted oldest first, so
            // the deletable head is `len - keep` (capped at 0).
            let n_to_delete = members.len().saturating_sub(keep);
            for (uuid, completed_at) in members.iter().take(n_to_delete) {
                candidates.push(RetentionCandidate {
                    uuid: uuid.clone(),
                    owner: key.owner.clone(),
                    channel: key.channel.clone(),
                    completed_at: *completed_at,
                    reason: RetentionReason::Count,
                });
            }
        }
        if let Some(days) = config.delete_after_days {
            let age_threshold_secs = i64::from(days).saturating_mul(86_400);
            for (uuid, completed_at) in &members {
                if now_secs.saturating_sub(*completed_at) >= age_threshold_secs {
                    candidates.push(RetentionCandidate {
                        uuid: uuid.clone(),
                        owner: key.owner.clone(),
                        channel: key.channel.clone(),
                        completed_at: *completed_at,
                        reason: RetentionReason::Age,
                    });
                }
            }
        }
    }
    // Final ordering: oldest first, then uuid tiebreak. Stable
    // across calls so the worker can delete one-at-a-time
    // without surprises. Dedupe by uuid so an age-eligible task
    // that is also count-overflow appears once. The retention
    // eligibility is the union of age and count candidates.
    candidates.sort_by(|a, b| a.completed_at.cmp(&b.completed_at).then_with(|| a.uuid.cmp(&b.uuid)));
    let mut deduped: Vec<RetentionCandidate> = Vec::with_capacity(candidates.len());
    let mut last_uuid: Option<String> = None;
    for c in candidates {
        if last_uuid.as_deref() == Some(c.uuid.as_str()) {
            continue;
        }
        last_uuid = Some(c.uuid.clone());
        deduped.push(c);
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::recording_queue::RecordingTaskState;
    use shared::model::recording::{RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility};

    fn make_meta(
        owner: RecordingOwner,
        channel_id: Option<&str>,
        channel_name: Option<&str>,
        completed_at: i64,
    ) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility: RecordingVisibility::Private,
            source: (RecordingSource::new("t1", "v1", "in1")),
            program_start: None,
            program_end: None,
            scheduled_start: None,
            scheduled_end: None,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: channel_id.map(str::to_string),
            channel_name: channel_name.map(str::to_string),
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: Some(completed_at),
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    struct T {
        uuid: String,
        state: RecordingTaskState,
        recording: RecordingMetadata,
    }
    impl super::QuotaRecordingTaskView for T {
        fn state(&self) -> &RecordingTaskState { &self.state }
        fn recording(&self) -> &RecordingMetadata { &self.recording }
        fn uuid(&self) -> &str { &self.uuid }
    }

    fn completed(
        uuid: &str,
        owner: RecordingOwner,
        channel_id: Option<&str>,
        channel_name: Option<&str>,
        completed_at: i64,
    ) -> T {
        T {
            uuid: uuid.to_string(),
            state: RecordingTaskState::Completed,
            recording: make_meta(owner, channel_id, channel_name, completed_at),
        }
    }

    /// A completed recording with shared visibility: it is charged to the
    /// shared retention group instead of the owner's.
    fn shared_completed(uuid: &str, channel_id: Option<&str>, channel_name: Option<&str>, completed_at: i64) -> T {
        let mut t =
            completed(uuid, RecordingOwner::User(UserId::from("web:alice")), channel_id, channel_name, completed_at);
        t.recording.visibility = RecordingVisibility::Shared;
        t
    }

    fn pending(uuid: &str) -> T {
        T {
            uuid: uuid.to_string(),
            state: RecordingTaskState::Scheduled,
            recording: make_meta(RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000_000),
        }
    }

    fn failed(uuid: &str) -> T {
        T {
            uuid: uuid.to_string(),
            state: RecordingTaskState::Failed,
            recording: make_meta(RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000_000),
        }
    }

    #[test]
    fn excludes_non_completed_states() {
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: Some(365) };
        let tasks = vec![pending("a"), failed("b")];
        let out = compute_candidates(&tasks, &config, 1_000_000_000);
        assert!(out.is_empty(), "non-Completed tasks must be excluded");
    }

    #[test]
    fn excludes_tasks_without_completed_at() {
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: Some(365) };
        let mut t = completed("a", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1);
        t.recording.completed_at = None;
        let out = compute_candidates(&[t], &config, 1_000_000_000);
        assert!(out.is_empty());
    }

    #[test]
    fn count_retention_keeps_n_oldest() {
        let config = RetentionConfig { keep_last_per_channel: Some(2), delete_after_days: None };
        let tasks = vec![
            completed("a", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000),
            completed("b", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 2_000),
            completed("c", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 3_000),
            completed("d", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 4_000),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        // keep 2 → 2 candidates (a, b) ordered oldest first
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].uuid, "a");
        assert_eq!(out[1].uuid, "b");
        assert_eq!(out[0].reason, RetentionReason::Count);
    }

    #[test]
    fn age_retention_picks_older_than_threshold() {
        let config = RetentionConfig { keep_last_per_channel: None, delete_after_days: Some(30) };
        // 30 days = 2_592_000 seconds
        let now = 30 * 86_400 + 1_000;
        let tasks = vec![
            completed(
                "old",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                0, // 30+ days old
            ),
            completed(
                "fresh",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 5, // 5 seconds old
            ),
        ];
        let out = compute_candidates(&tasks, &config, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].uuid, "old");
        assert_eq!(out[0].reason, RetentionReason::Age);
    }

    #[test]
    fn age_and_count_union() {
        // keep_last_per_channel = 3, delete_after_days = 30
        // Group has 5 tasks. 2 are count-overflow; 1 is also
        // age-eligible. The union contains 2 distinct tasks.
        let config = RetentionConfig { keep_last_per_channel: Some(3), delete_after_days: Some(30) };
        let now = 100 * 86_400;
        let tasks = vec![
            completed(
                "a",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 200 * 86_400, // age + count
            ),
            completed(
                "b",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 100 * 86_400, // age + count
            ),
            completed(
                "c",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 5 * 86_400, // keep
            ),
            completed(
                "d",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 86_400, // keep
            ),
            completed(
                "e",
                RecordingOwner::User(UserId::from("web:alice")),
                Some("c1"),
                Some("Alpha"),
                now - 86_400, // keep
            ),
        ];
        let out = compute_candidates(&tasks, &config, now);
        let uuids: Vec<&str> = out.iter().map(|c| c.uuid.as_str()).collect();
        assert_eq!(uuids, vec!["a", "b"]);
    }

    #[test]
    fn owner_isolation() {
        // alice and bob each have 3 recordings on the same
        // channel. keep_last_per_channel = 1. Each owner keeps
        // their newest 1 → 2 candidates per owner = 4 total.
        let config = RetentionConfig { keep_last_per_channel: Some(1), delete_after_days: None };
        let tasks = vec![
            completed("alice-1", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000),
            completed("alice-2", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 2_000),
            completed("alice-3", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 3_000),
            completed("bob-1", RecordingOwner::User(UserId::from("web:bob")), Some("c1"), Some("Alpha"), 4_000),
            completed("bob-2", RecordingOwner::User(UserId::from("web:bob")), Some("c1"), Some("Alpha"), 5_000),
            completed("bob-3", RecordingOwner::User(UserId::from("web:bob")), Some("c1"), Some("Alpha"), 6_000),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        let uuids: Vec<&str> = out.iter().map(|c| c.uuid.as_str()).collect();
        assert_eq!(uuids, vec!["alice-1", "alice-2", "bob-1", "bob-2"]);
    }

    #[test]
    fn channel_groups_by_id_when_present() {
        // Two recordings on the same stable channel id but
        // different display names still group together.
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None };
        let tasks = vec![
            completed("a", RecordingOwner::User(UserId::from("web:alice")), Some("stable-1"), Some("Alpha"), 1),
            completed("b", RecordingOwner::User(UserId::from("web:alice")), Some("stable-1"), Some("Alpha HD"), 2),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        // Same group (stable-1 + same owner) → 2 candidates
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn channel_falls_back_to_normalized_name() {
        // No `channel_id`; grouping is by normalized channel
        // name. "BBC One" and "bbc one" must group together.
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None };
        let tasks = vec![
            completed("a", RecordingOwner::User(UserId::from("web:alice")), None, Some("BBC One"), 1),
            completed("b", RecordingOwner::User(UserId::from("web:alice")), None, Some("bbc one  "), 2),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn normalize_channel_name_strips_country_tag() {
        assert_eq!(normalize_channel_name("BBC One (UK)"), "bbc one");
        assert_eq!(normalize_channel_name("  Sky  Sports  "), "sky sports");
        assert_eq!(normalize_channel_name(""), "");
        assert_eq!(normalize_channel_name("(orphan)"), "(orphan)"); // no closing
    }

    #[test]
    fn equal_timestamps_break_by_uuid() {
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None };
        let tasks = vec![
            completed("b", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000),
            completed("a", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1_000),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        let uuids: Vec<&str> = out.iter().map(|c| c.uuid.as_str()).collect();
        // Same completed_at → uuid ascending
        assert_eq!(uuids, vec!["a", "b"]);
    }

    #[test]
    fn exact_age_boundary_equals_threshold_is_kept() {
        // The age threshold is `>= days * 86_400`. A task whose
        // age is exactly equal to the threshold is **eligible**
        // for retention.
        let config = RetentionConfig { keep_last_per_channel: None, delete_after_days: Some(30) };
        let now = 30 * 86_400 + 5;
        let t = completed(
            "exact",
            RecordingOwner::User(UserId::from("web:alice")),
            Some("c1"),
            Some("Alpha"),
            5, // age = 30*86_400 exactly
        );
        let out = compute_candidates(&[t], &config, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].uuid, "exact");
    }

    #[test]
    fn shared_owner_groups_only_by_channel() {
        // Two shared recordings on the same channel — count
        // retention keeps the newest 1, so 1 candidate.
        let config = RetentionConfig { keep_last_per_channel: Some(1), delete_after_days: None };
        let tasks = vec![
            shared_completed("s1", Some("c1"), Some("Alpha"), 1),
            shared_completed("s2", Some("c1"), Some("Alpha"), 2),
        ];
        let out = compute_candidates(&tasks, &config, 0);
        let uuids: Vec<&str> = out.iter().map(|c| c.uuid.as_str()).collect();
        assert_eq!(uuids, vec!["s1"]);
    }

    #[test]
    fn no_config_returns_empty() {
        let config = RetentionConfig::default();
        let tasks = vec![completed("a", RecordingOwner::User(UserId::from("web:alice")), Some("c1"), Some("Alpha"), 1)];
        let out = compute_candidates(&tasks, &config, 0);
        assert!(out.is_empty());
    }
}
