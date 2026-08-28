use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A user was refused a connection because their limits were full.
///
/// `ActiveUser` reports connects and disconnects; a refusal is neither, so
/// the one outcome a user actually complains about was the one nothing
/// published. The admission ladder models it fully - it is the result after
/// every eviction strategy declines - and then returned it to the caller and
/// no one else.
///
/// Sits with the auth events rather than the streaming ones: "who was turned
/// away" is the same question as "who signed in", and takes the same
/// `UserRead` permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionDenied {
    pub username: Arc<str>,
    /// The address the request was attributed to. Only as trustworthy as the
    /// forwarding headers the server was configured to believe - the same
    /// caveat [`AuthAuditEvent`](crate::model::AuthAuditEvent) carries.
    pub client_ip: Arc<str>,
    /// The hard limit that was reached. Zero means unlimited.
    pub max_connections: u32,
    /// The soft limit that was reached. Zero means none configured.
    pub soft_connections: u16,
}

impl ConnectionDenied {
    #[must_use]
    pub fn new(username: Arc<str>, client_ip: Arc<str>, max_connections: u32, soft_connections: u16) -> Self {
        Self { username, client_ip, max_connections, soft_connections }
    }

    /// Per user: a client that has hit its limit retries, and each retry is
    /// refused the same way.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("connection-denied:{}", self.username) }
}
