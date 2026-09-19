use std::error::Error;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Debug, Clone, thiserror::Error)]
pub enum StreamError {
    #[error("Reqwest error: {message}")]
    Reqwest { message: String, class: &'static str, status: Option<u16> },
    #[error("IO error: {0}")]
    StdIo(String),
    #[error("Content decoding error: {0}")]
    ContentDecoding(String),
    // ReceiverClosed,
    #[error("Receiver error {0}")]
    ReceiverError(#[from] BroadcastStreamRecvError),
    #[error("LockError: {0}")]
    LockError(String),
    #[error("Stream: {0}")]
    Stream(String),
    #[error("MalformedPacket: {0}")]
    MalformedPacket(String),
    #[error("InvalidTimestamp: {0}")]
    InvalidTimestamp(String),
    #[error("SyncLoss: {0}")]
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
            Self::ContentDecoding(_) => "content_decoding",
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

#[cfg(test)]
mod tests {
    use super::StreamError;

    #[test]
    fn reqwest_stream_error_exposes_provider_failure_metadata() {
        let err = StreamError::Reqwest { message: "upstream failed".to_string(), class: "http_5xx", status: Some(503) };

        assert_eq!(err.provider_error_class(), "http_5xx");
        assert_eq!(err.provider_http_status(), Some(503));
    }
}
