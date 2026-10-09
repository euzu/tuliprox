use super::{RecordingOwner, RecordingSource, RecordingVisibility, UserId};

/// EPG episode metadata with tri-state airing provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct EpgEpisodeMetadata {
    pub programme_id: Option<String>,
    pub series_id: Option<String>,
    pub episode_id: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    #[serde(default)]
    pub airing: AiringStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiringStatus {
    #[default]
    Unknown,
    New,
    Repeat,
}

/// Lifecycle notification marker persisted alongside the task. At-most-once
/// delivery is enforced by recording this marker in the same queue mutation
/// as the lifecycle transition; the notification adapter is gated on it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationMarkerKind {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationMarker {
    pub kind: NotificationMarkerKind,
    pub attempted_at: i64,
}

impl NotificationMarker {
    pub fn new(kind: NotificationMarkerKind, attempted_at: i64) -> Self { Self { kind, attempted_at } }
}

/// Recurring-rule provenance. `rule_id` and `occurrence_key` are immutable for
/// the life of a task; the rule may be deleted but the task retains the
/// historical values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct RecordingProvenance {
    pub rule_id: Option<String>,
    pub occurrence_key: Option<String>,
}

/// Terminal state of a task before deletion began. Only `Completed`, `Failed`,
/// and `Cancelled` are valid previous states; the worker rejects transitions
/// from `Scheduled`/`Queued`/`Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeletionPreviousState {
    Completed,
    Failed,
    Cancelled,
}

impl DeletionPreviousState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Recording metadata. Every recording task carries exactly one of these;
/// the programme interval fields are populated for `RecordingKind::Live`
/// only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordingMetadata {
    pub owner: RecordingOwner,
    pub visibility: RecordingVisibility,
    /// Server-owned source identity. Never client supplied.
    pub source: RecordingSource,
    /// Original EPG interval (Unix seconds). Live only.
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    /// Effective padded interval (Unix seconds). When the program is
    /// currently airing, this is clamped to `now..scheduled_end`. Live only.
    pub scheduled_start: Option<i64>,
    pub scheduled_end: Option<i64>,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub program_title: Option<String>,
    pub epg: Option<EpgEpisodeMetadata>,
    /// Playlist group of the recorded item. Organised layouts file the
    /// recording under it.
    pub group: Option<String>,
    /// Series name shared by every episode of a series. Series only.
    pub series_name: Option<String>,
    #[serde(default)]
    pub provenance: RecordingProvenance,
    /// Final relative path below the recording root. `None` until the task
    /// is reserved or finalized.
    pub relative_path: Option<String>,
    /// Worker-owned partial path while a recording is in progress.
    pub partial_relative_path: Option<String>,
    /// Strong `ETag` captured from the first response, so a resume can prove
    /// the provider has not replaced the file underneath a partial download.
    /// Weak validators are never stored: they only promise semantic
    /// equivalence, so two matching weak tags can still be different bytes.
    /// Internal; never projected into a DTO.
    #[serde(default)]
    pub resume_etag: Option<String>,
    /// `Last-Modified` captured from the first response. Used only when no
    /// strong `ETag` was offered.
    #[serde(default)]
    pub resume_last_modified: Option<String>,
    /// Conservative quota reservation. `0` once the task completes.
    pub reserved_bytes: u64,
    /// Measured partial/final size. Authoritative once the task completes.
    pub measured_bytes: u64,
    /// Actual completion timestamp.
    pub completed_at: Option<i64>,
    /// Persisted at-most-once notification markers.
    #[serde(default)]
    pub notification_markers: Vec<NotificationMarker>,
    /// Terminal-only previous state while a deletion is in progress. `None`
    /// outside the `Deleting` transitional state.
    pub deleting_previous_state: Option<DeletionPreviousState>,
}

impl RecordingMetadata {
    /// Build metadata for a scheduled live recording.
    pub fn new_live(
        owner: RecordingOwner,
        visibility: RecordingVisibility,
        source: RecordingSource,
        program_start: i64,
        program_end: i64,
        pre_roll_secs: u64,
        post_roll_secs: u64,
    ) -> Self {
        let scheduled_start = program_start.saturating_sub(pre_roll_secs as i64);
        let scheduled_end = program_end.saturating_add(post_roll_secs as i64);
        Self {
            owner,
            visibility,
            source,
            program_start: Some(program_start),
            program_end: Some(program_end),
            scheduled_start: Some(scheduled_start),
            scheduled_end: Some(scheduled_end),
            pre_roll_secs,
            post_roll_secs,
            channel_id: None,
            channel_name: None,
            program_title: None,
            epg: None,
            provenance: RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            resume_etag: None,
            resume_last_modified: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: Vec::new(),
            group: None,
            series_name: None,
            deleting_previous_state: None,
        }
    }

    /// Build metadata for an immediate VOD or series-episode transfer. These
    /// have no programme window and no padding.
    pub fn new_media(
        owner: RecordingOwner,
        visibility: RecordingVisibility,
        source: RecordingSource,
        title: String,
    ) -> Self {
        Self {
            program_start: None,
            program_end: None,
            scheduled_start: None,
            scheduled_end: None,
            program_title: Some(title),
            ..Self::new_live(owner, visibility, source, 0, 0, 0, 0)
        }
    }

    pub fn is_deleting(&self) -> bool { self.deleting_previous_state.is_some() }

    pub fn owner_id(&self) -> &UserId { self.owner.user_id() }

    /// Filename component of the final relative path, if one is reserved.
    pub fn filename(&self) -> Option<&str> { self.relative_path.as_deref().and_then(extract_filename_component) }
}

fn extract_filename_component(relative_path: &str) -> Option<&str> {
    let trimmed = relative_path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    std::path::Path::new(trimmed).file_name().and_then(|s| s.to_str())
}
