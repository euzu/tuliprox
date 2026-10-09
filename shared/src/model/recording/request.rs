use super::{EpgEpisodeMetadata, UserId};

/// Recording owner. Every task is owned by exactly one user; there is no
/// unowned or admin-legacy record.
///
/// Adjacently tagged so the newtype `User(UserId)` serializes with the
/// content field name `user`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "user", rename_all = "snake_case")]
pub enum RecordingOwner {
    User(UserId),
}

impl RecordingOwner {
    pub fn user_id(&self) -> &UserId {
        let Self::User(uid) = self;
        uid
    }
}

/// Recording visibility. Shared recordings are visible to every user with
/// `recording.read`; private recordings are visible only to the owner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingVisibility {
    #[default]
    Private,
    Shared,
}

/// Server-owned source identifiers. The stream URL is never accepted from a
/// client; the server resolves it from these identifiers at execute time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordingSource {
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    #[serde(default)]
    pub cluster: super::super::XtreamCluster,
}

impl RecordingSource {
    pub fn new(target_id: impl Into<String>, virtual_id: impl Into<String>, input_name: impl Into<String>) -> Self {
        Self {
            target_id: target_id.into(),
            virtual_id: virtual_id.into(),
            input_name: input_name.into(),
            cluster: super::super::XtreamCluster::Live,
        }
    }

    pub fn with_cluster(mut self, cluster: super::super::XtreamCluster) -> Self {
        self.cluster = cluster;
        self
    }
}

/// Server-owned source identifiers as a client submits them.
///
/// This is the request half of [`RecordingSource`]: the client names the
/// catalog item, and the server resolves the stream URL from it at execute
/// time. A URL is never accepted from a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSourceRequest {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: super::super::XtreamCluster,
    pub input_name: String,
}

/// The one create-recording wire contract, shared by the REST handler and
/// the frontend client so the two cannot drift.
///
/// Unknown fields are rejected: a client that sends a field this build does
/// not understand has a different idea of what it is asking for, and
/// silently ignoring it would record the wrong thing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRecordingRequest {
    pub source: RecordingSourceRequest,
    pub program_title: String,
    /// Programme interval, Live only. Rejected for VOD and series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_end: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_roll_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_roll_secs: Option<u64>,
    pub visibility: RecordingVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg: Option<EpgEpisodeMetadata>,
}
