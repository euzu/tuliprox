pub mod recording_catalog_access;
pub mod recording_conflict;
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
pub mod recording_supervisor;
pub mod recording_worker;
pub mod recording_worker_runner;
pub mod recording_ws;

pub use self::{
    recording_catalog_access::*, recording_conflict::*, recording_currently_airing::*, recording_deletion::*,
    recording_disk::*, recording_edit::*, recording_notification::*,
    recording_notification_adapter::*,
    recording_observability::*, recording_occurrence::*, recording_quota::*, recording_reconciliation::*,
    recording_retention::*, recording_rule_scheduler::*, recording_rule_service::*, recording_security::*,
    recording_service::*, recording_supervisor::*, recording_worker::*, recording_worker_runner::*, recording_ws::*,
};
