//! Recording mutation service.

use super::{recording_ctx::RecordingCtx, recording_source_resolution as source_resolution};
use crate::{
    recording::recording_queue::{PersistedRecordingTask, RecordingQueue},
    recording_quota::QuotaPool,
};
use std::sync::Arc;
use tuliprox_core::model::AppConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffectiveRecordingWindow {
    scheduled_start: i64,
    scheduled_end: i64,
    execution_start: i64,
    remaining_duration_secs: u64,
}

/// Recording mutation boundary. Holds the queue and app config directly
/// so the server's root state does not carry a back-reference to the service.
pub struct RecordingService {
    recordings: Arc<RecordingQueue>,
    app_config: Arc<AppConfig>,
}

/// Primitives extracted from `RecordingMetadata` during the immutable
/// analysis pass. Carries only the fields the post-borrow code needs
/// so we never clone the full `RecordingMetadata` (which holds
/// several `Option<String>` / `Vec` allocations).
struct EditSnapshot {
    pool: QuotaPool,
    merged_pre: u64,
    merged_post: u64,
    channel_changed_now: bool,
    current_start: Option<i64>,
    current_end: Option<i64>,
    current_reserved: u64,
    is_live: bool,
    /// Another entry holds the same media, and with it the same file path.
    shares_media: bool,
}

/// A rule-materialized recording exactly as it was before the cancel.
#[derive(Debug, Clone)]
pub struct CancelledRuleRecording {
    origin: CancelOrigin,
    task: PersistedRecordingTask,
}

impl std::fmt::Debug for RecordingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingService").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;

mod conflicts;
mod control;
mod create;
mod deletion;
mod edit;
mod error;
mod filenames;
mod identity;
mod input;
mod rules;
mod window;
#[cfg(test)]
use conflicts::collect_demand_points_for_provider;
pub use conflicts::{quota_limits_from_config, ConflictPreviewRequest};
#[cfg(test)]
use control::is_future_rule_recording;
use control::CancelOrigin;
pub use create::IdempotencyRequest;
pub use error::ServiceError;
pub use filenames::sanitize_filename_component;
#[cfg(test)]
use filenames::{validate_reserved_filename, MAX_FILENAME_COMPONENT_BYTES};
pub(crate) use identity::recording_identity;
pub use identity::{recording_identity_key, RecordingIdentity};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use input::{
    CreateMediaRecordingInput, CreateRecordingInput, EditRecordingPatch, RecordingSourceInput, RecordingTaskView,
};
