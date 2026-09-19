use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Which end of the process lifetime this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerLifecycleState {
    /// The listener is bound and the server is serving.
    Started,
    /// A shutdown signal arrived; background services are being cancelled.
    ShuttingDown,
}

/// The server started or is stopping.
///
/// `system.started` and `system.shutdown` were registered in the
/// notification registry - and documented as available events - from the day
/// it was written, with nothing anywhere in the tree emitting them. An
/// operator who subscribed to either got silence.
///
/// One payload with two kinds rather than two variants, so a subscriber can
/// ask for restarts alone and a plugin sees one shape for "the process
/// lifetime changed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLifecycleEvent {
    pub state: ServerLifecycleState,
    /// The build that is running.
    pub version: Arc<str>,
    /// The bound `host:port`. Only meaningful on [`ServerLifecycleState::Started`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Arc<str>>,
    /// What triggered the stop - the signal name. Only meaningful on
    /// [`ServerLifecycleState::ShuttingDown`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Arc<str>>,
}

impl ServerLifecycleEvent {
    #[must_use]
    pub fn started(version: Arc<str>, address: Arc<str>) -> Self {
        Self { state: ServerLifecycleState::Started, version, address: Some(address), reason: None }
    }

    #[must_use]
    pub fn shutting_down(version: Arc<str>, reason: Arc<str>) -> Self {
        Self { state: ServerLifecycleState::ShuttingDown, version, address: None, reason: Some(reason) }
    }
}
