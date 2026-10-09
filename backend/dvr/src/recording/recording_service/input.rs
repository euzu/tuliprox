use super::ServiceError;
use crate::recording::recording_queue::RecordingTaskState;
use shared::model::{
    recording::{RecordingProvenance, RecordingVisibility},
    RecordingKind, UserId, XtreamCluster,
};

/// Server-resolved identifiers that the recording system needs. The
/// caller never sees the URL; the service resolves it from these.
#[derive(Debug, Clone)]
pub struct RecordingSourceInput {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: XtreamCluster,
    pub input_name: String,
}

impl RecordingSourceInput {
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.target_id.trim().is_empty() || self.virtual_id.trim().is_empty() || self.input_name.trim().is_empty() {
            return Err(ServiceError::InvalidSource);
        }
        Ok(())
    }
}

/// Input for `RecordingService::create_recording`.
#[derive(Debug, Clone)]
pub struct CreateRecordingInput {
    pub source: RecordingSourceInput,
    pub program_title: String,
    pub program_start: i64,
    pub program_end: i64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub visibility: RecordingVisibility,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    /// Playlist group of the channel, when the caller already resolved it.
    /// `None` makes the service look it up for organised layouts.
    pub group: Option<String>,
    pub provenance: RecordingProvenance,
    pub epg: Option<shared::model::recording::EpgEpisodeMetadata>,
}

impl CreateRecordingInput {
    pub fn validate(&self) -> Result<(), ServiceError> {
        self.source.validate()?;
        if self.program_end.checked_sub(self.program_start).is_none_or(|duration| duration <= 0) {
            return Err(ServiceError::InvalidInterval);
        }
        Ok(())
    }
}

/// Input for `RecordingService::create_media_recording_idempotent`: an
/// immediate VOD or series transfer. Title, extension and grouping come from
/// the playlist entry the caller already resolved.
#[derive(Debug, Clone)]
pub struct CreateMediaRecordingInput {
    pub source: RecordingSourceInput,
    pub title: String,
    pub extension: String,
    pub visibility: RecordingVisibility,
    pub group: Option<String>,
    pub series_name: Option<String>,
}

impl CreateMediaRecordingInput {
    pub fn validate(&self) -> Result<RecordingKind, ServiceError> {
        self.source.validate()?;
        if self.extension.trim_start_matches('.').trim().is_empty() {
            return Err(ServiceError::InvalidPath);
        }
        match self.source.cluster {
            XtreamCluster::Video => Ok(RecordingKind::Vod),
            XtreamCluster::Series => Ok(RecordingKind::Series),
            XtreamCluster::Live => Err(ServiceError::InvalidSource),
        }
    }
}

/// Output of `RecordingService::create_recording` and friends.
#[derive(Debug, Clone)]
pub struct RecordingTaskView {
    pub uuid: String,
    pub owner_id: UserId,
    pub visibility: RecordingVisibility,
    pub filename_preview: String,
    pub start_at: Option<i64>,
    pub duration_secs: Option<u64>,
    pub state: RecordingTaskState,
}

/// Input for `RecordingService::edit_recording`.
#[derive(Debug, Clone, Default)]
pub struct EditRecordingPatch {
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    pub pre_roll_secs: Option<u64>,
    pub post_roll_secs: Option<u64>,
    pub program_title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
}
