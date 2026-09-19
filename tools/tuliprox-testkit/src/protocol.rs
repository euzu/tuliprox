use crate::TestkitError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const SCHEMA_VERSION: u16 = 1;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self { Self(value.into()) }
        }
    };
}

id_type!(RunId);
id_type!(AgentId);
id_type!(ActorId);
id_type!(PlaybackId);
id_type!(CommandId);
id_type!(OriginStreamId);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub schema_version: u16,
    pub run_id: RunId,
    pub run_generation: u64,
    pub message_id: String,
    pub agent_id: AgentId,
    pub agent_boot_id: String,
    pub source_sequence: u64,
    pub caused_by_command_id: Option<CommandId>,
    pub local_elapsed_nanos: u64,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn validate(&self) -> Result<(), TestkitError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(TestkitError::Protocol(format!("unsupported schema version {}", self.schema_version)));
        }
        if self.run_id.0.is_empty() || self.message_id.is_empty() || self.agent_id.0.is_empty() {
            return Err(TestkitError::Protocol("empty envelope identity".to_owned()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    ConfigureRun {
        lease_millis: u64,
    },
    StartPlayback {
        playback_id: PlaybackId,
        url: String,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        expected_run_id: Option<RunId>,
        #[serde(default)]
        expected_marker: Option<u32>,
        #[serde(default)]
        required_frames: Option<u64>,
    },
    StopPlayback {
        playback_id: PlaybackId,
    },
    StopAll,
    RenewRunLease {
        lease_millis: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    CustomVideo(crate::custom_video::CustomVideoKind),
    HttpStatus(u16),
    ClosedBeforeFirstByte,
    Other(String),
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CustomVideo(kind) => write!(f, "custom-video({kind:?})"),
            Self::HttpStatus(status) => write!(f, "http-status({status})"),
            Self::ClosedBeforeFirstByte => write!(f, "closed-before-first-byte"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PlaybackOutcome {
    Streaming { frames: u64, bytes: u64 },
    AdmissionRejected { reason: RejectionReason },
    HttpError { status: u16 },
    TransportError { message: String },
    InvalidData { message: String },
    UnexpectedEof { frames: u64, bytes: u64 },
    IdleTimeout,
    ExplicitStop,
    InfrastructureError { message: String },
}

impl PlaybackOutcome {
    #[must_use]
    pub fn is_streaming(&self) -> bool { matches!(self, Self::Streaming { .. }) }

    #[must_use]
    pub fn is_rejected(&self) -> bool { matches!(self, Self::AdmissionRejected { .. }) }
}

impl std::fmt::Display for PlaybackOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Streaming { frames, bytes } => write!(f, "streaming({frames} frames, {bytes} bytes)"),
            Self::AdmissionRejected { reason } => write!(f, "rejected({reason})"),
            Self::HttpError { status } => write!(f, "http_error({status})"),
            Self::TransportError { message } => write!(f, "transport_error({message})"),
            Self::InvalidData { message } => write!(f, "invalid_data({message})"),
            Self::UnexpectedEof { frames, bytes } => write!(f, "unexpected_eof({frames} frames, {bytes} bytes)"),
            Self::IdleTimeout => write!(f, "idle_timeout"),
            Self::ExplicitStop => write!(f, "explicit_stop"),
            Self::InfrastructureError { message } => write!(f, "infrastructure_error({message})"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlaybackEvent {
    CommandAccepted {
        command_id: CommandId,
    },
    HeadersReceived {
        playback_id: PlaybackId,
        status: u16,
    },
    FirstValidFrame {
        playback_id: PlaybackId,
        sequence: u64,
    },
    Progress {
        playback_id: PlaybackId,
        frames: u64,
        bytes: u64,
    },
    Terminal {
        playback_id: PlaybackId,
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        typed_outcome: Option<PlaybackOutcome>,
    },
    LeaseExpired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentMessage {
    Hello { hostname: String, supported_protocols: Vec<String>, maximum_listeners: u32 },
    Ready,
    Event { event: PlaybackEvent },
    Heartbeat,
}

#[derive(Debug, Clone)]
pub struct RunLease {
    generation: u64,
    expires_at: tokio::time::Instant,
}

impl RunLease {
    #[must_use]
    pub fn new(generation: u64, duration: Duration) -> Self {
        Self { generation, expires_at: tokio::time::Instant::now() + duration }
    }

    pub fn renew(&mut self, generation: u64, duration: Duration) -> Result<(), TestkitError> {
        if generation != self.generation {
            return Err(TestkitError::Protocol("stale run generation".to_owned()));
        }
        self.expires_at = tokio::time::Instant::now() + duration;
        Ok(())
    }

    #[must_use]
    pub fn expired(&self) -> bool { tokio::time::Instant::now() >= self.expires_at }

    #[must_use]
    pub const fn deadline(&self) -> tokio::time::Instant { self.expires_at }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_schema() {
        let envelope = Envelope {
            schema_version: 2,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: "message".to_owned(),
            agent_id: AgentId::new("agent"),
            agent_boot_id: "boot".to_owned(),
            source_sequence: 0,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: Command::StopAll,
        };
        assert!(envelope.validate().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn lease_expiry_and_renewal_use_monotonic_time() {
        let mut lease = RunLease::new(1, Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(!lease.expired());
        lease.renew(1, Duration::from_secs(5)).unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(!lease.expired());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(lease.expired());
    }
}
