use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyCloseReason {
    Completed,
    ClientDisconnected,
    Evicted,
    Stalled,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OriginEventKind {
    TcpAccepted {
        conn_id: u64,
        remote_addr: String,
    },
    RequestStarted {
        conn_id: u64,
        request_id: u64,
        method: String,
        path: String,
        range: Option<String>,
        user_agent: Option<String>,
        account: Option<String>,
    },
    BodyStarted {
        conn_id: u64,
        request_id: u64,
        content_length: Option<u64>,
    },
    BodyClosed {
        conn_id: u64,
        request_id: u64,
        bytes_emitted: u64,
        duration_ms: u64,
        reason: BodyCloseReason,
    },
    TcpClosed {
        conn_id: u64,
        duration_ms: u64,
        bytes_read: u64,
        bytes_written: u64,
        reason: String,
    },
    LimitRejected {
        conn_id: u64,
        request_id: u64,
        limit: usize,
        current_active: usize,
    },
    EvictionTriggered {
        evicted_conn_id: u64,
        evicted_request_id: u64,
        triggering_conn_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginEvent {
    pub seq: u64,
    pub timestamp_ns: u128,
    pub run_id: String,
    #[serde(flatten)]
    pub kind: OriginEventKind,
}

impl OriginEvent {
    #[must_use]
    pub fn new(seq: u64, run_id: impl Into<String>, kind: OriginEventKind) -> Self {
        let timestamp_ns = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
        Self { seq, timestamp_ns, run_id: run_id.into(), kind }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginStats {
    pub active_tcp_connections: usize,
    pub max_active_tcp_connections: usize,
    pub active_body_connections: usize,
    pub max_active_body_connections: usize,
    pub total_tcp_connections: u64,
    pub total_requests: u64,
    pub total_bytes_emitted: u64,
}

/// What the emulated Stalker portal was asked for during a run.
///
/// Playback resolution is only observable through the portal: a scenario asserts that the
/// requested channel's `create_link` was attempted, that a stale-session refusal was
/// followed by a re-handshake and a retry, and which markers actually resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalkerPortalStats {
    pub handshakes: u64,
    /// Every `create_link` request, refusals included.
    pub create_links: u64,
    pub token_refusals: u64,
    /// Markers the portal answered with a stream URL, sorted and deduplicated.
    pub resolved_markers: Vec<u32>,
    pub resolved_descriptor_markers: Vec<u32>,
    pub resolved_raw_command_markers: Vec<u32>,
}

#[must_use]
pub fn redact_path_and_query(raw: &str) -> String {
    let Some((path, query)) = raw.split_once('?') else {
        return raw.to_owned();
    };
    let redacted_query: Vec<String> = query
        .split('&')
        .map(|pair| {
            if let Some((key, _)) = pair.split_once('=') {
                let lower = key.to_ascii_lowercase();
                if lower.contains("pass")
                    || lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("key")
                {
                    return format!("{key}=<redacted>");
                }
            }
            pair.to_owned()
        })
        .collect();
    format!("{path}?{}", redacted_query.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_query_parameters() {
        let path = "/movie/stream.mkv?user=alice&password=secret123&token=abc&run=test-run";
        let redacted = redact_path_and_query(path);
        assert!(redacted.contains("user=alice"));
        assert!(redacted.contains("password=<redacted>"));
        assert!(redacted.contains("token=<redacted>"));
        assert!(redacted.contains("run=test-run"));
    }
}
