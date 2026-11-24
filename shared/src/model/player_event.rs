use serde::{Deserialize, Serialize};

/// Reason why an active stream was ended by the server or user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEndedReason {
    Kicked,
    Preempted,
    /// The stream was stopped voluntarily by client request (e.g. `DELETE /api/v1/me/streams/{uid}`).
    Stopped,
}

/// Terminal state of a finished recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFinishedState {
    Completed,
    Failed,
}

/// JSON event frames delivered to authenticated player WebSocket connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayerEvent {
    Connected { server_time: i64 },
    StreamEnded { uid: u32, virtual_id: u32, reason: StreamEndedReason },
    FailoverMemberSwitched { group_virtual_id: u32, from_member_vid: u32, to_member_vid: u32 },
    RecordingFinished { id: String, state: RecordingFinishedState },
    ConnectionDenied { max_connections: u32 },
    ResyncRequired,
    Error { code: String },
}

/// Messages sent from player clients to the server over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayerClientMessage {
    Auth { token: String },
}
