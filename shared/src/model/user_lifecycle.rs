//! User account lifecycle, as an event.
//!
//! Creating, editing or deleting an API-proxy user changed persisted state
//! and told nobody: the panel wrote `api_proxy.yml`, swapped the in-memory
//! config and returned `200`. An operator could not be notified, and an
//! audit trail had nothing to read.
//!
//! This is the payload for that. One record with a [`UserLifecycleState`],
//! following [`ProviderAccountEvent`](crate::model::ProviderAccountEvent) and
//! [`RecordingLifecycleMessage`](crate::model::RecordingLifecycleMessage):
//! one payload, several `EventKind`s, so a subscriber can ask for deletions
//! alone.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What happened to the account.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserLifecycleState {
    Created,
    Updated,
    Deleted,
}

/// An API-proxy user was created, changed or removed.
///
/// # What is deliberately absent
///
/// The password, token and any proxy credentials. This record reaches
/// notification channels — Telegram, a webhook, a shell command — and
/// several of them are third-party services with their own logging. An
/// account identity is what a lifecycle notification needs; the secret is
/// not, so it is not in the type at all rather than being redacted at each
/// site that renders one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserLifecycleEvent {
    pub username: Arc<str>,
    /// The target the account belongs to.
    pub target: Arc<str>,
    pub state: UserLifecycleState,
}

impl UserLifecycleEvent {
    #[must_use]
    pub fn new(username: Arc<str>, target: Arc<str>, state: UserLifecycleState) -> Self {
        Self { username, target, state }
    }

    /// One account's news is one piece of news, however many times a panel
    /// re-saves it. Scoped to the state as well as the account so a delete
    /// is not suppressed by the update that preceded it.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        let state = match self.state {
            UserLifecycleState::Created => "created",
            UserLifecycleState::Updated => "updated",
            UserLifecycleState::Deleted => "deleted",
        };
        format!("user:{}:{}:{state}", self.target, self.username)
    }
}
