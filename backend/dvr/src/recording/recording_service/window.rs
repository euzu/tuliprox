use super::{EffectiveRecordingWindow, ServiceError};
use crate::recording_edit::PaddingBounds;

pub(super) fn effective_recording_window(
    program_start: i64,
    program_end: i64,
    pre_roll_secs: u64,
    post_roll_secs: u64,
    now: i64,
) -> Result<EffectiveRecordingWindow, ServiceError> {
    if program_end <= program_start {
        return Err(ServiceError::InvalidInterval);
    }
    let pre_roll = i64::try_from(pre_roll_secs).map_err(|_| ServiceError::PaddingLimitExceeded)?;
    let post_roll = i64::try_from(post_roll_secs).map_err(|_| ServiceError::PaddingLimitExceeded)?;
    let scheduled_start = program_start.saturating_sub(pre_roll);
    let scheduled_end = program_end.saturating_add(post_roll);
    let execution_start = now.max(scheduled_start);
    // `scheduled_end >= execution_start` because both come from
    // saturating arithmetic on a non-empty interval, so the cast is
    // safe and the only remaining error is the degenerate
    // already-finished window.
    let remaining = scheduled_end.saturating_sub(execution_start);
    if remaining <= 0 {
        return Err(ServiceError::InvalidInterval);
    }
    let remaining_duration_secs = remaining.cast_unsigned();
    Ok(EffectiveRecordingWindow { scheduled_start, scheduled_end, execution_start, remaining_duration_secs })
}

pub(super) fn padding_bounds(recording: Option<&tuliprox_core::model::RecordingConfig>) -> PaddingBounds {
    recording.map_or(
        PaddingBounds {
            max_pre_roll_secs: shared::model::default_recording_max_pre_roll_secs(),
            max_post_roll_secs: shared::model::default_recording_max_post_roll_secs(),
        },
        |config| PaddingBounds {
            max_pre_roll_secs: config.max_pre_roll_secs,
            max_post_roll_secs: config.max_post_roll_secs,
        },
    )
}
