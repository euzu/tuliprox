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
    pub provider_id: Option<String>,
    pub is_group: bool,
}

impl RecordingSourceInput {
    pub fn new(target_id: impl Into<String>, virtual_id: impl Into<String>, input_name: impl Into<String>) -> Self {
        Self {
            target_id: target_id.into(),
            virtual_id: virtual_id.into(),
            cluster: XtreamCluster::Live,
            input_name: input_name.into(),
            provider_id: None,
            is_group: false,
        }
    }

    pub fn with_cluster(mut self, cluster: XtreamCluster) -> Self {
        self.cluster = cluster;
        self
    }

    pub fn with_provider_id(mut self, provider_id: Option<String>) -> Self {
        self.provider_id = provider_id;
        self
    }

    pub fn with_is_group(mut self, is_group: bool) -> Self {
        self.is_group = is_group;
        self
    }

    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.target_id.trim().is_empty() || self.virtual_id.trim().is_empty() || self.input_name.trim().is_empty() {
            return Err(ServiceError::InvalidSource);
        }
        Ok(())
    }
}

impl From<&shared::model::recording::RecordingSource> for RecordingSourceInput {
    fn from(source: &shared::model::recording::RecordingSource) -> Self {
        Self {
            target_id: source.target_id.clone(),
            virtual_id: source.virtual_id.clone(),
            cluster: source.cluster,
            input_name: source.input_name.clone(),
            provider_id: None,
            is_group: false,
        }
    }
}

impl From<shared::model::recording::RecordingSource> for RecordingSourceInput {
    fn from(source: shared::model::recording::RecordingSource) -> Self {
        Self {
            target_id: source.target_id,
            virtual_id: source.virtual_id,
            cluster: source.cluster,
            input_name: source.input_name,
            provider_id: None,
            is_group: false,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording::RecordingSource;

    #[test]
    fn client_supplied_provider_id_and_is_group_are_ignored() {
        let json = serde_json::json!({
            "target_id": "target-1",
            "virtual_id": "42",
            "input_name": "input-1",
            "cluster": "Live",
            "provider_id": "client-provided-id",
            "is_group": true
        });
        let client_source: RecordingSource = serde_json::from_value(json).expect("deserialize");
        assert_eq!(client_source.provider_id.as_deref(), Some("client-provided-id"));
        assert!(client_source.is_group);

        let input_from_ref = RecordingSourceInput::from(&client_source);
        assert_eq!(input_from_ref.provider_id, None);
        assert!(!input_from_ref.is_group);

        let input_from_val = RecordingSourceInput::from(client_source);
        assert_eq!(input_from_val.provider_id, None);
        assert!(!input_from_val.is_group);
    }
}
