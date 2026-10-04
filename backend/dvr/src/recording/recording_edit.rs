//! Validation for edits to upcoming recordings.
//!
//! Only recordings that have not started can be edited; the transition graph
//! decides which states those are. The edit itself runs inside the queue
//! mutation in `RecordingService::edit_recording`, which derives the padded
//! window, re-checks quota and keeps rule provenance untouched. This module
//! holds the checks that need no queue state.

use crate::recording::recording_transition;
use shared::model::RecordingTaskState;

/// Why an edit is refused.
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

/// Configured padding bounds (mirrors `RecordingConfigDto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaddingBounds {
    pub max_pre_roll_secs: u64,
    pub max_post_roll_secs: u64,
}

/// `true` when a recording in `state` can still have its plan changed.
///
/// Delegates to the transition graph so the edit cutoff cannot drift from the
/// action the UI offers.
pub fn state_is_editable(state: RecordingTaskState) -> bool { recording_transition::can_edit(state) }

pub fn validate_padding(pre_roll_secs: u64, post_roll_secs: u64, bounds: PaddingBounds) -> Result<(), EditError> {
    if pre_roll_secs > bounds.max_pre_roll_secs || post_roll_secs > bounds.max_post_roll_secs {
        return Err(EditError::PaddingLimitExceeded);
    }
    Ok(())
}

/// Whether an edit moves the recording to another channel. A `None` means
/// "no change", so the channel only counts as changed when the edit sets a
/// different value. A changed channel invalidates the EPG / episode metadata.
pub fn channel_changed(
    new_channel_id: Option<&str>,
    new_channel_name: Option<&str>,
    current_channel_id: Option<&str>,
    current_channel_name: Option<&str>,
) -> bool {
    new_channel_id.is_some_and(|id| Some(id) != current_channel_id)
        || new_channel_name.is_some_and(|name| Some(name) != current_channel_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> PaddingBounds { PaddingBounds { max_pre_roll_secs: 900, max_post_roll_secs: 1800 } }

    #[test]
    fn state_is_editable_accepts_only_upcoming_states() {
        for state in [
            RecordingTaskState::Scheduled,
            RecordingTaskState::Queued,
            RecordingTaskState::WaitingForCapacity,
            RecordingTaskState::RetryWaiting,
        ] {
            assert!(state_is_editable(state), "{}", state.label());
        }
        for state in [
            RecordingTaskState::Running,
            RecordingTaskState::Paused,
            RecordingTaskState::Completed,
            RecordingTaskState::Failed,
            RecordingTaskState::Cancelled,
        ] {
            assert!(!state_is_editable(state), "{}", state.label());
        }
    }

    #[test]
    fn validate_padding_rejects_values_above_configured_maximum() {
        assert_eq!(validate_padding(901, 0, bounds()), Err(EditError::PaddingLimitExceeded));
        assert_eq!(validate_padding(0, 1_801, bounds()), Err(EditError::PaddingLimitExceeded));
        assert!(validate_padding(900, 1_800, bounds()).is_ok());
    }

    #[test]
    fn channel_changed_only_when_id_or_name_differ() {
        assert!(!channel_changed(None, None, Some("a"), Some("A")));
        assert!(!channel_changed(Some("a"), Some("A"), Some("a"), Some("A")));
        assert!(channel_changed(Some("b"), None, Some("a"), Some("A")));
        assert!(channel_changed(None, Some("B"), Some("a"), Some("A")));
    }
}
