use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A metadata update cycle that ended with work it could not finish.
///
/// The counterpart to `EventMessage::InputMetadataUpdatesCompleted`, which
/// only fires when a cycle drained *with changes*. Without this, an input
/// whose resolves fail every time emits `InputMetadataUpdatesStarted` and
/// then nothing at all - indistinguishable on the bus from one that is still
/// working through a long queue.
///
/// Reported once per cycle rather than once per task. A provider that has
/// stopped answering fails every item behind it, and an operator wants to
/// hear "this input is not resolving" once, not once per title.
///
/// # Redaction
///
/// `last_error` is a provider error message and may carry a URL with
/// credentials in it. The emitter passes it through
/// [`sanitize_sensitive_info`](crate::utils::sanitize_sensitive_info) before
/// constructing this record, for the same reason
/// [`StreamProbeFailure`](crate::model::StreamProbeFailure) documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataUpdateFailure {
    /// The input whose cycle this was.
    pub input: Arc<str>,
    /// How many tasks exhausted their retries during the cycle.
    pub failed_tasks: usize,
    /// Whether anything at all resolved. A cycle can both produce changes and
    /// exhaust tasks; that is a partial failure, not a clean run.
    pub had_changes: bool,
    /// The last error a failing task reported, already sanitized.
    pub last_error: Option<String>,
}

impl MetadataUpdateFailure {
    #[must_use]
    pub fn new(input: Arc<str>, failed_tasks: usize, had_changes: bool, last_error: Option<String>) -> Self {
        Self { input, failed_tasks, had_changes, last_error }
    }

    /// Per input, matching `StreamProbeFailure::dedup_key`: one broken
    /// provider should not page someone once per refresh cycle forever.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("metadata-update:{}", self.input) }
}
