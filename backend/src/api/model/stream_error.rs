use std::error::Error;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Debug, Clone)]
pub enum StreamError {
    Reqwest { message: String, class: &'static str, status: Option<u16> },
    StdIo(String),
    // ReceiverClosed,
    ReceiverError(BroadcastStreamRecvError),
    LockError(String),
    Stream(String),
    MalformedPacket(String),
    InvalidTimestamp(String),
    SyncLoss(String),
}

impl StreamError {
    fn reqwest_error_has_dns_source(err: &reqwest::Error) -> bool {
        let mut source = err.source();
        while let Some(current) = source {
            let lowered = current.to_string().to_ascii_lowercase();
            if lowered.contains("dns")
                || lowered.contains("failed to lookup address information")
                || lowered.contains("name or service not known")
                || lowered.contains("no such host")
                || lowered.contains("temporary failure in name resolution")
            {
                return true;
            }
            source = current.source();
        }
        false
    }

    fn classify_reqwest(err: &reqwest::Error) -> &'static str {
        if let Some(status) = err.status() {
            if status.is_client_error() {
                return "http_4xx";
            }
            if status.is_server_error() {
                return "http_5xx";
            }
            if status.is_redirection() {
                return "http_3xx";
            }
            return "http_other";
        }
        if err.is_timeout() {
            return "timeout";
        }
        if Self::reqwest_error_has_dns_source(err) {
            return "dns";
        }
        if err.is_connect() {
            return "connect";
        }
        if err.is_redirect() {
            return "redirect";
        }
        if err.is_body() {
            return "body";
        }
        if err.is_decode() {
            return "decode";
        }
        if err.is_request() {
            return "request";
        }
        "unknown"
    }

    pub fn reqwest(err: &reqwest::Error) -> Self {
        Self::Reqwest {
            message: err.to_string(),
            class: Self::classify_reqwest(err),
            status: err.status().map(|status| status.as_u16()),
        }
    }

    pub fn provider_error_class(&self) -> &'static str {
        match self {
            Self::Reqwest { class, .. } => class,
            Self::StdIo(_) => "io",
            Self::ReceiverError(_) => "receiver",
            Self::LockError(_) => "lock",
            Self::Stream(_) => "stream",
            Self::MalformedPacket(_) => "malformed_packet",
            Self::InvalidTimestamp(_) => "invalid_timestamp",
            Self::SyncLoss(_) => "sync_loss",
        }
    }

    pub fn provider_http_status(&self) -> Option<u16> {
        match self {
            Self::Reqwest { status, .. } => *status,
            _ => None,
        }
    }
}

impl std::error::Error for StreamError {}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Reqwest { message, .. } => write!(f, "Reqwest error: {message}"),
            StreamError::StdIo(e) => write!(f, "IO error: {e}"),
            // StreamError::ReceiverClosed =>  write!(f, "Receiver closed"),
            StreamError::ReceiverError(e) => write!(f, "Receiver error {e}"),
            StreamError::Stream(e) => write!(f, "Stream: {e}"),
            StreamError::LockError(e) => write!(f, "LockError: {e}"),
            StreamError::MalformedPacket(e) => write!(f, "MalformedPacket: {e}"),
            StreamError::InvalidTimestamp(e) => write!(f, "InvalidTimestamp: {e}"),
            StreamError::SyncLoss(e) => write!(f, "SyncLoss: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StreamError;

    #[test]
    fn reqwest_stream_error_exposes_provider_failure_metadata() {
        let err = StreamError::Reqwest {
            message: "upstream failed".to_string(),
            class: "http_5xx",
            status: Some(503),
        };

        assert_eq!(err.provider_error_class(), "http_5xx");
        assert_eq!(err.provider_http_status(), Some(503));
    }
}
