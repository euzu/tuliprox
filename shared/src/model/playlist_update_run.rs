use crate::model::PlaylistUpdateState;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, sync::Arc};

/// Stable identity shared by every event and request belonging to one playlist update run.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistUpdateRunId(Arc<str>);

impl PlaylistUpdateRunId {
    /// Generates a process-independent 128-bit run identity.
    #[must_use]
    pub fn generate() -> Self {
        let high = fastrand::u64(..);
        let low = fastrand::u64(..);
        Self(format!("{high:016x}{low:016x}").into())
    }
}

impl AsRef<str> for PlaylistUpdateRunId {
    fn as_ref(&self) -> &str { &self.0 }
}

impl fmt::Display for PlaylistUpdateRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str(&self.0) }
}

impl From<&str> for PlaylistUpdateRunId {
    fn from(value: &str) -> Self { Self(value.into()) }
}

impl From<String> for PlaylistUpdateRunId {
    fn from(value: String) -> Self { Self(value.into()) }
}

/// Monotonic order assigned when a playlist update enters actual execution.
///
/// This is deliberately separate from [`PlaylistUpdateRunId`]: request
/// acceptance order is not execution order for the bounded serial queue.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistUpdateRunOrder(u64);

impl PlaylistUpdateRunOrder {
    #[must_use]
    pub const fn get(self) -> u64 { self.0 }
}

impl From<u64> for PlaylistUpdateRunOrder {
    fn from(value: u64) -> Self { Self(value) }
}

/// Terminal state of one playlist update run.
///
/// Deserialization also accepts the legacy bare [`PlaylistUpdateState`] payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlaylistUpdateRunStateEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<PlaylistUpdateRunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_order: Option<PlaylistUpdateRunOrder>,
    pub state: PlaylistUpdateState,
}

impl PlaylistUpdateRunStateEvent {
    #[must_use]
    pub fn correlated(
        run_id: PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        state: PlaylistUpdateState,
    ) -> Self {
        Self { run_id: Some(run_id), execution_order: Some(execution_order), state }
    }

    #[must_use]
    pub const fn uncorrelated(state: PlaylistUpdateState) -> Self {
        Self { run_id: None, execution_order: None, state }
    }
}

impl<'de> Deserialize<'de> for PlaylistUpdateRunStateEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CompatiblePayload {
            Legacy(PlaylistUpdateState),
            Correlated {
                #[serde(default)]
                run_id: Option<PlaylistUpdateRunId>,
                #[serde(default)]
                execution_order: Option<PlaylistUpdateRunOrder>,
                state: PlaylistUpdateState,
            },
        }

        Ok(match CompatiblePayload::deserialize(deserializer)? {
            CompatiblePayload::Legacy(state) => Self::uncorrelated(state),
            CompatiblePayload::Correlated { run_id, execution_order, state } => Self { run_id, execution_order, state },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateRunStateEvent};
    use crate::model::PlaylistUpdateState;

    #[test]
    fn playlist_update_progress_terminal_event_accepts_legacy_state_payload() -> Result<(), serde_json::Error> {
        let event = serde_json::from_str::<PlaylistUpdateRunStateEvent>(r#""Success""#)?;

        assert_eq!(event, PlaylistUpdateRunStateEvent::uncorrelated(PlaylistUpdateState::Success));
        Ok(())
    }

    #[test]
    fn playlist_update_progress_terminal_event_roundtrips_run_identity() -> Result<(), serde_json::Error> {
        let event = PlaylistUpdateRunStateEvent::correlated(
            "run-17".into(),
            PlaylistUpdateRunOrder::from(17),
            PlaylistUpdateState::Partial,
        );

        let encoded = serde_json::to_string(&event)?;
        let decoded = serde_json::from_str::<PlaylistUpdateRunStateEvent>(&encoded)?;

        assert_eq!(decoded, event);
        assert_eq!(decoded.run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-17"));
        assert_eq!(decoded.execution_order.map(PlaylistUpdateRunOrder::get), Some(17));
        Ok(())
    }

    #[test]
    fn playlist_update_progress_terminal_event_accepts_correlated_payload_without_execution_order(
    ) -> Result<(), serde_json::Error> {
        let event = serde_json::from_str::<PlaylistUpdateRunStateEvent>(r#"{"run_id":"run-12-1","state":"Success"}"#)?;

        assert_eq!(event.run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-12-1"));
        assert_eq!(event.execution_order, None);
        assert_eq!(event.state, PlaylistUpdateState::Success);
        Ok(())
    }
}
