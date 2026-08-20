//! Upcoming recording edit validation.
//!
//! Edits are allowed only in `Scheduled`, `Queued`,
//! `WaitingForCapacity`, `RetryWaiting`. `Downloading`, terminal
//! states, and `Deleting` are immutable. The invariants:
//!
//! - Recalculate the conservative reservation using the remaining
//!   duration where the start is in the past (currently-airing).
//! - Re-reserve a derived output path atomically and release the
//!   old reservation only when commit succeeds. The queue-mutation
//!   boundary does that work; this module exposes the *pure*
//!   validation that runs inside the boundary.
//! - Preserve immutable `rule_id` and `occurrence_key`.
//! - Clear EPG / episode metadata on channel/provider change unless
//!   the server verifies a fresh matching programme payload.
//! - Return advisory conflict warnings with the committed edit
//!   response.
//!
//! This module owns the pure helpers. The actual queue-mutation
//! wiring (path reservation, quota, atomic persist) lands with the
//! queue transaction.
//!
//! The helpers are tested in isolation. They are public so the
//! queue-mutation boundary can call them once the wiring lands; the
//! `dead_code` allowance below is the test surface.


use shared::model::recording::{RecordingMetadata, RecordingVisibility};

/// The set of states a recording can be in for an edit to be
/// accepted.
pub const EDITABLE_STATES: &[&str] = &["Scheduled", "Queued", "WaitingForCapacity", "RetryWaiting"];

/// Edit-time error taxonomy. Stable wire codes live in
/// `RecordingService::ServiceError`; this enum is the *pure*
/// validation's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The recording is in a state that does not allow edits
    /// (active / terminal / `Deleting`).
    StateNotEditable,
    /// `program_end - program_start` is non-positive or the patch
    /// leaves the interval in an invalid shape.
    InvalidInterval,
    /// A padding field exceeds the configured maximum.
    PaddingLimitExceeded,
    /// The patch would clear the rule provenance or occurrence key
    /// (forbidden — both are immutable).
    ProvenanceCleared,
    /// The channel/provider changed but no matching programme
    /// payload was supplied to refresh the EPG / episode metadata.
    ChannelChangedWithoutProgramme,
}

/// A patch of editable values. The serializer deserializes only the
/// fields that the API accepts; immutable fields (`rule_id`,
/// `occurrence_key`, `owner`, `visibility`, `source`) are never
/// part of the patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditPatch {
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    pub pre_roll_secs: Option<u64>,
    pub post_roll_secs: Option<u64>,
    pub program_title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
}

/// Configured padding bounds (mirrors `RecordingConfigDto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaddingBounds {
    pub max_pre_roll_secs: u64,
    pub max_post_roll_secs: u64,
}

/// The current state label. The form strings are
/// `Scheduled` / `Queued` / `WaitingForCapacity` / `RetryWaiting` /
/// `Downloading` / `Completed` / `Failed` / `Cancelled` /
/// `Deleting(<previous>)`. The current state for the existing
/// `DownloadState` variants is reduced to a string here.
pub fn state_is_editable(state_label: &str) -> bool { EDITABLE_STATES.contains(&state_label) }

/// Pure: validate the merged interval (patch overlaid on current) and
/// the patch's padding bounds. Validation runs against the merged
/// `program_start`/`program_end` so a patch that only sets `end`
/// still has to produce a valid interval against the stored `start`.
pub fn validate_patch(
    patch: &EditPatch,
    current: &RecordingMetadata,
    bounds: PaddingBounds,
) -> Result<(), EditError> {
    let merged_start = patch.program_start.or(current.program_start);
    let merged_end = patch.program_end.or(current.program_end);
    if let (Some(start), Some(end)) = (merged_start, merged_end) {
        if end <= start {
            return Err(EditError::InvalidInterval);
        }
    }
    validate_padding(
        patch.pre_roll_secs.unwrap_or(current.pre_roll_secs),
        patch.post_roll_secs.unwrap_or(current.post_roll_secs),
        bounds,
    )?;
    Ok(())
}

pub fn validate_padding(
    pre_roll_secs: u64,
    post_roll_secs: u64,
    bounds: PaddingBounds,
) -> Result<(), EditError> {
    if pre_roll_secs > bounds.max_pre_roll_secs || post_roll_secs > bounds.max_post_roll_secs {
        return Err(EditError::PaddingLimitExceeded);
    }
    Ok(())
}

/// Pure: detect the channel/provider change rule. When the patch
/// changes `channel_id` or `channel_name` and the caller did not
/// supply a fresh programme payload, the EPG / episode metadata
/// must be cleared. A `None` in the patch means "no change", so the
/// channel only counts as changed when the patch actually sets a
/// different value.
pub fn channel_changed(patch: &EditPatch, current_channel_id: Option<&str>, current_channel_name: Option<&str>) -> bool {
    if let Some(new_id) = patch.channel_id.as_deref() {
        if Some(new_id) != current_channel_id {
            return true;
        }
    }
    if let Some(new_name) = patch.channel_name.as_deref() {
        if Some(new_name) != current_channel_name {
            return true;
        }
    }
    false
}

/// Pure: derive the new interval and padded window from the
/// `current` metadata and the patch.
pub fn apply_interval_patch(
    current: &RecordingMetadata,
    patch: &EditPatch,
) -> (i64, i64, i64, i64) {
    let program_start = patch.program_start.unwrap_or_else(|| current.program_start.unwrap_or(0));
    let program_end = patch.program_end.unwrap_or_else(|| current.program_end.unwrap_or(0));
    let pre = patch.pre_roll_secs.unwrap_or(current.pre_roll_secs);
    let post = patch.post_roll_secs.unwrap_or(current.post_roll_secs);
    let scheduled_start = program_start.saturating_sub(pre.cast_signed());
    let scheduled_end = program_end.saturating_add(post.cast_signed());
    (program_start, program_end, scheduled_start, scheduled_end)
}

/// Pure: verify the patch never clears the rule provenance or
/// occurrence key. The patch surface does not include them, so this
/// is a defensive check that the caller did not smuggle them in.
pub fn patch_preserves_provenance(patch: &EditPatch) -> bool {
    // The patch is typed; clearing the rule provenance would
    // require either `Option<None>` for fields that the type does
    // not have, or a separate constructor that explicitly nulls
    // them. The type system already prevents that. This helper
    // exists so the queue-mutation boundary has a single named
    // gate to call.
    let _ = patch;
    true
}

/// Decide whether the patch's EPG / episode metadata should be
/// cleared. Clear unless the caller supplied a fresh matching
/// programme payload. The frontend either sends `program_title`
/// (refresh signal) or the channel matches the current one.
pub fn epg_metadata_should_be_cleared(
    patch: &EditPatch,
    current: &RecordingMetadata,
    fresh_programme_supplied: bool,
) -> bool {
    if fresh_programme_supplied {
        return false;
    }
    let current_id = current.channel_id.as_deref();
    let current_name = current.channel_name.as_deref();
    channel_changed(patch, current_id, current_name)
}

/// Stable wire code for an edit error. Mirrors the
/// `recording_*` error code family.
pub fn edit_error_code(err: &EditError) -> &'static str {
    match err {
        EditError::StateNotEditable => "recording_state_not_editable",
        EditError::InvalidInterval => "recording_invalid_interval",
        EditError::PaddingLimitExceeded => "recording_padding_limit_exceeded",
        EditError::ProvenanceCleared => "recording_provenance_immovable",
        EditError::ChannelChangedWithoutProgramme => "recording_channel_changed_without_programme",
    }
}

/// The visibility is not part of the editable surface. The boundary calls
/// this helper to assert the caller did not change it via a forged
/// payload — `requested` is ignored and the current visibility is always
/// returned. Callers must not surface `requested` back to the caller.
pub fn visibility_unchanged(
    current_visibility: RecordingVisibility,
    _requested: Option<RecordingVisibility>,
) -> RecordingVisibility {
    current_visibility
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> PaddingBounds { PaddingBounds { max_pre_roll_secs: 900, max_post_roll_secs: 1800 } }

    #[test]
    fn state_is_editable_accepts_only_upcoming_states() {
        for s in EDITABLE_STATES {
            assert!(state_is_editable(s));
        }
        assert!(!state_is_editable("Downloading"));
        assert!(!state_is_editable("Completed"));
        assert!(!state_is_editable("Failed"));
        assert!(!state_is_editable("Cancelled"));
        assert!(!state_is_editable("Deleting(Completed)"));
    }

    fn current_meta() -> RecordingMetadata {
        // Baseline current metadata: program 100..500, no padding.
        // Tests overlay a patch on top of this and assert the merged
        // interval is validated.
        let mut m = RecordingMetadata::for_legacy_admin(100, 400);
        m.pre_roll_secs = 0;
        m.post_roll_secs = 0;
        m
    }

    #[test]
    fn validate_patch_rejects_inverted_interval() {
        let patch = EditPatch { program_start: Some(200), program_end: Some(100), ..Default::default() };
        assert_eq!(validate_patch(&patch, &current_meta(), bounds()), Err(EditError::InvalidInterval));
    }

    #[test]
    fn validate_patch_rejects_merged_inverted_interval() {
        // Patch only sets end; current start is 100. Setting end to 50
        // produces an inverted merged interval that must be rejected.
        let patch = EditPatch { program_end: Some(50), ..Default::default() };
        assert_eq!(validate_patch(&patch, &current_meta(), bounds()), Err(EditError::InvalidInterval));
    }

    #[test]
    fn validate_patch_rejects_pre_roll_above_max() {
        let patch = EditPatch { pre_roll_secs: Some(901), ..Default::default() };
        assert_eq!(validate_patch(&patch, &current_meta(), bounds()), Err(EditError::PaddingLimitExceeded));
    }

    #[test]
    fn validate_patch_rejects_post_roll_above_max() {
        let patch = EditPatch { post_roll_secs: Some(1801), ..Default::default() };
        assert_eq!(validate_patch(&patch, &current_meta(), bounds()), Err(EditError::PaddingLimitExceeded));
    }

    #[test]
    fn validate_padding_rejects_values_above_configured_maximum() {
        assert_eq!(validate_padding(901, 0, bounds()), Err(EditError::PaddingLimitExceeded));
        assert_eq!(validate_padding(0, 1_801, bounds()), Err(EditError::PaddingLimitExceeded));
        assert!(validate_padding(900, 1_800, bounds()).is_ok());
    }

    #[test]
    fn validate_patch_accepts_padded_extensions() {
        let patch = EditPatch { program_end: Some(1_000), post_roll_secs: Some(1_800), ..Default::default() };
        assert!(validate_patch(&patch, &current_meta(), bounds()).is_ok());
    }

    #[test]
    fn channel_changed_only_when_id_or_name_differ() {
        let patch = EditPatch::default();
        assert!(!channel_changed(&patch, Some("a"), Some("A")));
        let patch = EditPatch { channel_id: Some("b".into()), ..Default::default() };
        assert!(channel_changed(&patch, Some("a"), Some("A")));
        let patch = EditPatch { channel_name: Some("B".into()), ..Default::default() };
        assert!(channel_changed(&patch, Some("a"), Some("A")));
    }

    #[test]
    fn apply_interval_patch_keeps_current_when_unset() {
        let meta = make_meta(100, 200, 0, 0);
        let patch = EditPatch::default();
        let (start, end, scheduled_start, scheduled_end) = apply_interval_patch(&meta, &patch);
        assert_eq!((start, end, scheduled_start, scheduled_end), (100, 200, 100, 200));
    }

    #[test]
    fn apply_interval_patch_uses_padding() {
        let meta = make_meta(100, 200, 0, 0);
        let patch = EditPatch { pre_roll_secs: Some(60), post_roll_secs: Some(120), ..Default::default() };
        let (_, _, scheduled_start, scheduled_end) = apply_interval_patch(&meta, &patch);
        assert_eq!(scheduled_start, 40);
        assert_eq!(scheduled_end, 320);
    }

    #[test]
    fn apply_interval_patch_handles_extreme_window_without_panicking() {
        let meta = make_meta(i64::MIN, i64::MAX, 0, 0);

        let interval = apply_interval_patch(&meta, &EditPatch::default());

        assert_eq!(interval, (i64::MIN, i64::MAX, i64::MIN, i64::MAX));
    }

    #[test]
    fn epg_metadata_should_be_cleared_when_channel_changes_without_payload() {
        let meta = make_meta(0, 0, 0, 0);
        let meta = RecordingMetadata { channel_id: Some("a".into()), channel_name: Some("A".into()), ..meta };
        let patch = EditPatch { channel_id: Some("b".into()), ..Default::default() };
        assert!(epg_metadata_should_be_cleared(&patch, &meta, false));
        assert!(!epg_metadata_should_be_cleared(&patch, &meta, true));
    }

    #[test]
    fn epg_metadata_preserved_when_channel_unchanged() {
        let meta = RecordingMetadata { channel_id: Some("a".into()), channel_name: Some("A".into()), ..make_meta(0, 0, 0, 0) };
        let patch = EditPatch::default();
        assert!(!epg_metadata_should_be_cleared(&patch, &meta, false));
    }

    #[test]
    fn patch_preserves_provenance_by_type() {
        // The patch type cannot carry rule_id or occurrence_key. The
        // helper is a defensive gate; the type system is the
        // primary defense.
        assert!(patch_preserves_provenance(&EditPatch::default()));
    }

    #[test]
    fn visibility_unchanged_keeps_current_when_none() {
        use shared::model::recording::RecordingVisibility;
        assert_eq!(
            visibility_unchanged(RecordingVisibility::Private, None),
            RecordingVisibility::Private
        );
    }

    #[test]
    fn edit_error_codes_are_stable() {
        assert_eq!(edit_error_code(&EditError::StateNotEditable), "recording_state_not_editable");
        assert_eq!(edit_error_code(&EditError::InvalidInterval), "recording_invalid_interval");
        assert_eq!(
            edit_error_code(&EditError::PaddingLimitExceeded),
            "recording_padding_limit_exceeded"
        );
    }

    /// Tiny helper so the test signatures stay short.
    fn make_meta(start: i64, end: i64, pre: u64, post: u64) -> RecordingMetadata {
        RecordingMetadata {
            owner: shared::model::recording::RecordingOwner::LegacyAdmin,
            visibility: shared::model::recording::RecordingVisibility::Private,
            source: None,
            program_start: Some(start),
            program_end: Some(end),
            scheduled_start: Some(start),
            scheduled_end: Some(end),
            pre_roll_secs: pre,
            post_roll_secs: post,
            channel_id: None,
            channel_name: None,
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: vec![],
            deleting_previous_state: None,
        }
    }
}
