//! Authentication decisions, as events.
//!
//! Sign-ins, sign-in failures and permission denials went to `warn!` and
//! `debug!` and nowhere else. An operator could not be told that an account
//! was being ground against, and nothing that subscribes to the bus - a
//! notification channel, a plugin, an audit sink - could see any of it. The
//! events that matter most for spotting an intrusion were the ones the bus
//! never carried.
//!
//! One payload with an [`AuthAuditOutcome`], following
//! [`UserLifecycleEvent`](crate::model::UserLifecycleEvent): several
//! `EventKind`s off one type, so a subscriber can ask for failures alone and
//! not be woken by every successful sign-in.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What the auth layer decided.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthAuditOutcome {
    /// Credentials verified and a token was issued.
    SignInSucceeded,
    /// Credentials were rejected.
    SignInFailed,
    /// The attempt was refused without checking credentials, because the
    /// caller is backing off after repeated failures.
    SignInThrottled,
    /// An authenticated principal asked for something its permissions do not
    /// cover.
    PermissionDenied,
}

impl AuthAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignInSucceeded => "sign_in_succeeded",
            Self::SignInFailed => "sign_in_failed",
            Self::SignInThrottled => "sign_in_throttled",
            Self::PermissionDenied => "permission_denied",
        }
    }
}

/// One authentication decision.
///
/// # What is deliberately absent
///
/// The password, the token, and any part of either. This record reaches
/// notification channels - Telegram, a webhook, a shell command - several of
/// which are third-party services with their own logging. A username, an
/// address and an outcome are what an audit trail needs; a credential is
/// not, so it is not in the type at all rather than being redacted at each
/// site that renders one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthAuditEvent {
    pub username: Arc<str>,
    /// The address the request was attributed to. Only as trustworthy as the
    /// forwarding headers the server was configured to believe.
    pub client_ip: Arc<str>,
    pub outcome: AuthAuditOutcome,
    /// The permission that was refused, for [`AuthAuditOutcome::PermissionDenied`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Arc<str>>,
}

impl AuthAuditEvent {
    #[must_use]
    pub fn sign_in_succeeded(username: Arc<str>, client_ip: Arc<str>) -> Self {
        Self { username, client_ip, outcome: AuthAuditOutcome::SignInSucceeded, permission: None }
    }

    #[must_use]
    pub fn sign_in_failed(username: Arc<str>, client_ip: Arc<str>) -> Self {
        Self { username, client_ip, outcome: AuthAuditOutcome::SignInFailed, permission: None }
    }

    #[must_use]
    pub fn sign_in_throttled(username: Arc<str>, client_ip: Arc<str>) -> Self {
        Self { username, client_ip, outcome: AuthAuditOutcome::SignInThrottled, permission: None }
    }

    #[must_use]
    pub fn permission_denied(username: Arc<str>, client_ip: Arc<str>, permission: Arc<str>) -> Self {
        Self { username, client_ip, outcome: AuthAuditOutcome::PermissionDenied, permission: Some(permission) }
    }

    /// One principal failing repeatedly from one address is one piece of
    /// news, not one per attempt - which is exactly the case that generates
    /// the most events. Scoped to the outcome so a success is not suppressed
    /// by the failures that preceded it.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        format!("auth:{}:{}:{}", self.outcome.as_str(), self.username, self.client_ip)
    }
}
