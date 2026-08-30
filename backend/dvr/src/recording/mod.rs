pub mod recording_catalog_access;
pub mod recording_conflict;
pub mod recording_ctx;
pub mod recording_currently_airing;
pub mod recording_deletion;
pub mod recording_disk;
pub mod recording_edit;
// Pure window arithmetic with no dependencies; moved to `shared::model` so the
// download path can use it without reaching into the recording subsystem.
pub use shared::model::recording_math;
pub mod recording_notification;
pub mod recording_notification_adapter;
pub mod recording_observability;
pub mod recording_occurrence;
pub mod recording_quota;
pub mod recording_reconciliation;
pub mod recording_retention;
pub mod recording_rule_scheduler;
pub mod recording_rule_service;
pub mod recording_security;
pub mod recording_service;
pub mod recording_source_resolution;
pub mod recording_supervisor;
pub mod recording_worker;
pub mod recording_worker_runner;
pub mod recording_ws;

pub use self::{
    recording_catalog_access::*, recording_conflict::*, recording_ctx::*, recording_currently_airing::*,
    recording_deletion::*, recording_disk::*, recording_edit::*, recording_notification::*,
    recording_notification_adapter::*, recording_observability::*, recording_occurrence::*, recording_quota::*,
    recording_reconciliation::*, recording_retention::*, recording_rule_scheduler::*, recording_rule_service::*,
    recording_security::*, recording_service::*, recording_source_resolution::*, recording_supervisor::*,
    recording_worker::*, recording_worker_runner::*, recording_ws::*,
};

#[cfg(test)]
pub(crate) fn make_test_meta(
    visibility: shared::model::recording::RecordingVisibility,
    owner: shared::model::recording::RecordingOwner,
    relative_path: Option<&str>,
) -> shared::model::RecordingMetadata {
    shared::model::RecordingMetadata {
        owner,
        visibility,
        source: None,
        program_start: Some(1_700_000_000),
        program_end: Some(1_700_003_600),
        scheduled_start: Some(1_700_000_000),
        scheduled_end: Some(1_700_003_600),
        pre_roll_secs: 0,
        post_roll_secs: 0,
        channel_id: Some("ch-1".into()),
        channel_name: Some("Channel 1".into()),
        program_title: Some("Programme".into()),
        epg: None,
        provenance: shared::model::recording::RecordingProvenance::default(),
        relative_path: relative_path.map(Into::into),
        partial_relative_path: None,
        reserved_bytes: 0,
        measured_bytes: 0,
        completed_at: None,
        notification_markers: vec![],
        deleting_previous_state: None,
    }
}
