//! Typed payloads of the events a plugin can subscribe to.
//!
//! The host hands `on-event` the wire name (`ev.kind`) and the event's JSON
//! (`ev.payload`). The structs here are hand-mirrored from
//! `shared/src/model/event` and are deliberately lenient: unknown fields are
//! ignored, absent optional fields default, and an enum value this SDK does not
//! know deserializes to `Unknown` rather than failing, so a newer server never
//! breaks an older plugin.
//!
//! ```ignore
//! fn on_event(ev: Event) -> Result<(), String> {
//!     match ev.typed()? {
//!         TypedEvent::PlaylistUpdate(update) if update.state == UpdateState::Failure => alert(&update),
//!         _ => {}
//!     }
//!     Ok(())
//! }
//! ```

use crate::wit::event_hooks::Event;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

/// Wire names of the events with a typed payload.
pub const PLAYLIST_UPDATE: &str = "playlist.update";
pub const USER_CONNECTION_CHANGED: &str = "user.connection.changed";
pub const STREAM_PROBE_FAILED: &str = "stream.probe.failed";
pub const RECORDING_CHANGED: &str = "recording.changed";
pub const SCHEDULED_TASK_FAILED: &str = "scheduled_task.failed";

/// `playlist.update`: what one playlist refresh did.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlaylistUpdate {
    /// Identity of the processing run (absent on legacy events).
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub execution_order: Option<u64>,
    pub state: UpdateState,
    /// Per-source statistics, as the server emits them (`inputs` / `targets`).
    #[serde(default)]
    pub stats: Vec<Value>,
    /// The aggregated error text, when the run reported errors.
    #[serde(default)]
    pub error: Option<String>,
}

/// The outcome of a playlist refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum UpdateState {
    Success,
    Failure,
    Partial,
    #[serde(other)]
    Unknown,
}

/// `user.connection.changed`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum UserConnectionChanged {
    /// A client connected or its stream changed.
    Updated(Box<StreamInfo>),
    /// A client disconnected (its socket address).
    Disconnected(String),
    DisconnectedStream {
        addr: String,
        #[serde(default)]
        uid: u32,
    },
    /// `(user_count, connection_count)`.
    Connections(usize, usize),
    #[serde(other)]
    Unknown,
}

/// The active stream of a connected user.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamInfo {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub username: String,
    /// The channel being watched, as the server emits it.
    #[serde(default)]
    pub channel: Value,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub addr: String,
    #[serde(default)]
    pub client_ip: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub country_code: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub started_at: u64,
}

/// `stream.probe.failed`: one stream did not probe.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamProbeFailed {
    /// The input whose stream this is.
    pub input: String,
    /// The stream's id within the input.
    #[serde(default)]
    pub unique_id: String,
    /// The probed URL, already sanitized by the server.
    #[serde(default)]
    pub url: String,
    pub reason: ProbeFailureReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFailureReason {
    NotFound,
    Unreachable,
    #[serde(other)]
    Unknown,
}

/// `scheduled_task.failed`: a scheduled task could not complete.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScheduledTaskFailed {
    /// The task type (`PlaylistUpdate`, `LibraryScan`, `GeoIpUpdate`, `PluginTask`).
    pub task: String,
    #[serde(default)]
    pub schedule: Option<String>,
    /// `plugin:<id>/<task>` for a plugin task.
    #[serde(default)]
    pub plugin_task: Option<String>,
    pub error: String,
}

/// An event, decoded as far as the SDK knows its kind.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedEvent {
    PlaylistUpdate(PlaylistUpdate),
    UserConnectionChanged(UserConnectionChanged),
    StreamProbeFailed(StreamProbeFailed),
    /// `recording.changed` carries no payload.
    RecordingChanged,
    ScheduledTaskFailed(ScheduledTaskFailed),
    /// A kind without a typed payload here: the wire name and its raw JSON.
    Other {
        kind: String,
        payload: Value,
    },
}

/// Decodes the JSON payload of the event `kind`.
pub fn decode(kind: &str, payload: &str) -> Result<TypedEvent, String> {
    fn parse<T: DeserializeOwned>(kind: &str, payload: &str) -> Result<T, String> {
        serde_json::from_str(payload).map_err(|err| format!("event `{kind}` payload: {err}"))
    }
    Ok(match kind {
        PLAYLIST_UPDATE => TypedEvent::PlaylistUpdate(parse(kind, payload)?),
        USER_CONNECTION_CHANGED => TypedEvent::UserConnectionChanged(parse(kind, payload)?),
        STREAM_PROBE_FAILED => TypedEvent::StreamProbeFailed(parse(kind, payload)?),
        SCHEDULED_TASK_FAILED => TypedEvent::ScheduledTaskFailed(parse(kind, payload)?),
        RECORDING_CHANGED => TypedEvent::RecordingChanged,
        _ => {
            TypedEvent::Other { kind: kind.to_string(), payload: serde_json::from_str(payload).unwrap_or(Value::Null) }
        }
    })
}

/// `ev.typed()` on the wire event.
pub trait EventExt {
    fn typed(&self) -> Result<TypedEvent, String>;
}

impl EventExt for Event {
    fn typed(&self) -> Result<TypedEvent, String> { decode(&self.kind, &self.payload) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playlist_update_decodes_leniently() {
        let json =
            r#"{"run_id":"abc","state":"Partial","stats":[{"inputs":[],"targets":[]}],"error":"x","new_field":1}"#;
        let TypedEvent::PlaylistUpdate(update) = decode(PLAYLIST_UPDATE, json).unwrap() else { panic!("kind") };
        assert_eq!(update.state, UpdateState::Partial);
        assert_eq!(update.run_id.as_deref(), Some("abc"));
        assert_eq!(update.stats.len(), 1);

        let bare = decode(PLAYLIST_UPDATE, r#"{"state":"Brand-new"}"#).unwrap();
        assert!(matches!(bare, TypedEvent::PlaylistUpdate(PlaylistUpdate { state: UpdateState::Unknown, .. })));
    }

    #[test]
    fn the_user_connection_variants_decode() {
        let counts = decode(USER_CONNECTION_CHANGED, r#"{"type":"connections","payload":[3,5]}"#).unwrap();
        assert_eq!(counts, TypedEvent::UserConnectionChanged(UserConnectionChanged::Connections(3, 5)));
        let gone = decode(USER_CONNECTION_CHANGED, r#"{"type":"disconnected","payload":"10.0.0.1:5000"}"#).unwrap();
        assert_eq!(
            gone,
            TypedEvent::UserConnectionChanged(UserConnectionChanged::Disconnected("10.0.0.1:5000".into()))
        );
        let updated = decode(
            USER_CONNECTION_CHANGED,
            r#"{"type":"updated","payload":{"uid":7,"username":"ann","addr":"10.0.0.1:5000","channel":{"title":"x"},"extra":true}}"#,
        )
        .unwrap();
        let TypedEvent::UserConnectionChanged(UserConnectionChanged::Updated(info)) = updated else {
            panic!("variant")
        };
        assert_eq!((info.uid, info.username.as_str()), (7, "ann"));
        let unknown = decode(USER_CONNECTION_CHANGED, r#"{"type":"kicked","payload":null}"#).unwrap();
        assert_eq!(unknown, TypedEvent::UserConnectionChanged(UserConnectionChanged::Unknown));
    }

    #[test]
    fn probe_failures_task_failures_and_recording_changes_decode() {
        let probe =
            decode(STREAM_PROBE_FAILED, r#"{"input":"p1","unique_id":"12","url":"http://x/***","reason":"not_found"}"#);
        assert!(matches!(
            probe,
            Ok(TypedEvent::StreamProbeFailed(StreamProbeFailed { reason: ProbeFailureReason::NotFound, .. }))
        ));
        let task = decode(
            SCHEDULED_TASK_FAILED,
            r#"{"task":"PluginTask","plugin_task":"plugin:demo/refresh","error":"boom"}"#,
        );
        assert!(
            matches!(task, Ok(TypedEvent::ScheduledTaskFailed(ScheduledTaskFailed { ref error, .. })) if error == "boom")
        );
        assert_eq!(decode(RECORDING_CHANGED, "null").unwrap(), TypedEvent::RecordingChanged);
    }

    #[test]
    fn other_kinds_keep_their_raw_payload_and_bad_payloads_name_the_kind() {
        let other = decode("system.disk.alert", r#"{"free":1}"#).unwrap();
        assert_eq!(
            other,
            TypedEvent::Other { kind: "system.disk.alert".into(), payload: serde_json::json!({"free": 1}) }
        );
        let err = decode(PLAYLIST_UPDATE, "{").unwrap_err();
        assert!(err.starts_with("event `playlist.update` payload:"));
    }
}
