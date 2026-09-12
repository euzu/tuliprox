use crate::model::{
    InputRefreshPolicy, LibraryScanResult, LibraryScanSummary, PersistedPlaylistUpdateTechnicalState,
    PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateState, XtreamCluster,
};
use serde::{Deserialize, Serialize};

/// Proven retrieval source selected for an input during one playlist-update run.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistUpdateDataSource {
    Cache,
    Provider,
}

/// Backend-owned result for one requested Xtream-compatible cluster.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistUpdateClusterDecision {
    Accepted,
    Rejected,
    TechnicalError,
}

/// Runtime facts proven for one cluster during a playlist-update run.
///
/// Population and quality values remain optional because not every provider
/// boundary exposes them. Consumers must not reconstruct missing values.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistUpdateClusterTelemetry {
    pub cluster: XtreamCluster,
    pub requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PlaylistUpdateDataSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PlaylistUpdateClusterDecision>,
    /// Proven technical result, independent of an earlier Quality decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technical_state: Option<PersistedPlaylistUpdateTechnicalState>,
}

/// Input-scoped runtime facts carried by the existing progress lifecycle.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistUpdateInputTelemetry {
    pub refresh_policy: InputRefreshPolicy,
    /// Set for inputs without cluster-specific sources, such as M3U.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PlaylistUpdateDataSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<PlaylistUpdateClusterTelemetry>,
}

/// Response for an accepted long-running operation.
///
/// Playlist updates return the identity of the queued run. Other operations and
/// legacy payloads may leave it empty.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationRunAccepted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<PlaylistUpdateRunId>,
}

impl OperationRunAccepted {
    #[must_use]
    pub fn playlist_update(run_id: PlaylistUpdateRunId) -> Self { Self { run_id: Some(run_id) } }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistUpdateProgressEvent {
    /// Stable update-run identity. Missing only on legacy payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<PlaylistUpdateRunId>,
    /// Actual serial execution order. Missing on accepted responses and legacy events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_order: Option<PlaylistUpdateRunOrder>,
    /// Stable input identity for input-scoped progress. Global and target-scoped
    /// processing steps leave this empty because they cannot be assigned to one input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<u16>,
    /// Input-local completion state when this progress line closes an input job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PlaylistUpdateState>,
    /// Optional structured input facts. Missing on legacy and text-only
    /// progress frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_telemetry: Option<PlaylistUpdateInputTelemetry>,
    /// Actual scanner result for this run/input, including unsuccessful scans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_scan_result: Option<LibraryScanResult>,
    pub target: String,
    pub message: String,
}

impl PlaylistUpdateProgressEvent {
    #[must_use]
    pub fn for_run_input(
        run_id: PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        input_id: u16,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            execution_order: Some(execution_order),
            input_id: Some(input_id),
            state: None,
            input_telemetry: None,
            library_scan_result: None,
            target: target.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn input_completed(
        run_id: PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        input_id: u16,
        state: PlaylistUpdateState,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            execution_order: Some(execution_order),
            input_id: Some(input_id),
            state: Some(state),
            input_telemetry: None,
            library_scan_result: None,
            target: target.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn for_run_global(
        run_id: PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            execution_order: Some(execution_order),
            input_id: None,
            state: None,
            input_telemetry: None,
            library_scan_result: None,
            target: target.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn for_input(input_id: u16, target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            run_id: None,
            execution_order: None,
            input_id: Some(input_id),
            state: None,
            input_telemetry: None,
            library_scan_result: None,
            target: target.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn global(target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            run_id: None,
            execution_order: None,
            input_id: None,
            state: None,
            input_telemetry: None,
            library_scan_result: None,
            target: target.into(),
            message: message.into(),
        }
    }

    /// Attaches input telemetry without creating another event family.
    #[must_use]
    pub fn with_input_telemetry(mut self, input_telemetry: PlaylistUpdateInputTelemetry) -> Self {
        self.input_telemetry = Some(input_telemetry);
        self
    }

    /// Carries the scanner's facts without changing acquisition or Quality telemetry.
    #[must_use]
    pub fn with_library_scan_result(mut self, result: LibraryScanResult) -> Self {
        self.library_scan_result = Some(result);
        self
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LibraryScanProgressEvent {
    pub summary: LibraryScanSummary,
}

#[cfg(test)]
mod tests {
    #[test]
    fn library_scan_result_roundtrips_with_run_identity_and_legacy_remains_unknown() {
        let result = crate::model::LibraryScanResult {
            files_scanned: 11,
            groups_scanned: 7,
            files_added: 3,
            files_updated: 2,
            files_removed: 1,
            errors: 4,
        };
        let event =
            super::PlaylistUpdateProgressEvent::for_run_input("library-run".into(), 8.into(), 17, "Library", "result")
                .with_library_scan_result(result.clone());
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: super::PlaylistUpdateProgressEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(decoded.library_scan_result, Some(result));
        let old: super::PlaylistUpdateProgressEvent =
            serde_json::from_str(r#"{"target":"Library","message":"scanning"}"#).unwrap();
        assert_eq!(old.library_scan_result, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("library_scan_result"));
    }

    use super::{
        OperationRunAccepted, PlaylistUpdateClusterDecision, PlaylistUpdateClusterTelemetry, PlaylistUpdateDataSource,
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent,
    };
    use crate::model::{
        InputRefreshPolicy, PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateState, XtreamCluster,
    };

    #[test]
    fn playlist_update_progress_legacy_payload_defaults_to_unscoped_identity() -> Result<(), serde_json::Error> {
        let event: PlaylistUpdateProgressEvent = serde_json::from_str(r#"{"target":"legacy","message":"working"}"#)?;

        assert_eq!(event.input_id, None);
        assert_eq!(event.run_id, None);
        assert_eq!(event.execution_order, None);
        assert_eq!(event.state, None);
        assert_eq!(event.input_telemetry, None);
        assert_eq!(event.target, "legacy");
        assert_eq!(event.message, "working");
        Ok(())
    }

    #[test]
    fn playlist_update_progress_input_identity_roundtrips() -> Result<(), serde_json::Error> {
        let event = PlaylistUpdateProgressEvent::input_completed(
            "run-17".into(),
            PlaylistUpdateRunOrder::from(23),
            17,
            PlaylistUpdateState::Partial,
            "provider",
            "working",
        );

        let encoded = serde_json::to_string(&event)?;
        let decoded = serde_json::from_str::<PlaylistUpdateProgressEvent>(&encoded)?;

        assert_eq!(decoded, event);
        assert_eq!(decoded.execution_order.map(PlaylistUpdateRunOrder::get), Some(23));
        Ok(())
    }

    #[test]
    fn playlist_update_progress_correlated_payload_without_execution_order_remains_compatible(
    ) -> Result<(), serde_json::Error> {
        let event = serde_json::from_str::<PlaylistUpdateProgressEvent>(
            r#"{"run_id":"run-12-1","input_id":7,"target":"provider","message":"working"}"#,
        )?;

        assert_eq!(event.run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-12-1"));
        assert_eq!(event.execution_order, None);
        assert_eq!(event.input_id, Some(7));
        Ok(())
    }

    #[test]
    fn playlist_update_progress_accepted_legacy_payload_has_no_run_identity() -> Result<(), serde_json::Error> {
        let accepted = serde_json::from_str::<OperationRunAccepted>("{}")?;

        assert_eq!(accepted.run_id, None);
        Ok(())
    }

    #[test]
    fn playlist_update_progress_accepted_response_roundtrips_run_identity() -> Result<(), serde_json::Error> {
        let accepted = OperationRunAccepted::playlist_update("run-accepted".into());

        let decoded = serde_json::from_str::<OperationRunAccepted>(&serde_json::to_string(&accepted)?)?;

        assert_eq!(decoded.run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-accepted"));
        Ok(())
    }

    #[test]
    fn pipeline_transparency_cluster_failure_is_optional_and_separate_from_quality() {
        let legacy = serde_json::json!({"cluster": XtreamCluster::Series, "requested": true, "decision": "accepted"});
        let mut telemetry: PlaylistUpdateClusterTelemetry = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(telemetry.technical_state, None);
        assert_eq!(serde_json::to_value(&telemetry).unwrap(), legacy);
        telemetry.technical_state = Some(super::PersistedPlaylistUpdateTechnicalState::Failed);
        let encoded = serde_json::to_value(&telemetry).unwrap();
        assert_eq!(encoded["decision"], "accepted");
        assert_eq!(encoded["technical_state"], "failed");
        assert_eq!(serde_json::from_value::<PlaylistUpdateClusterTelemetry>(encoded).unwrap(), telemetry);
    }

    #[test]
    fn pipeline_transparency_input_telemetry_roundtrips_on_existing_progress_event() -> Result<(), serde_json::Error> {
        let telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Video,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Provider),
                baseline_count: Some(12_543),
                candidate_count: Some(217),
                active_count: Some(12_543),
                threshold: Some(90),
                quality: Some(1),
                decision: Some(PlaylistUpdateClusterDecision::Rejected),
                technical_state: None,
            }],
        };
        let event = PlaylistUpdateProgressEvent::input_completed(
            "run-telemetry".into(),
            PlaylistUpdateRunOrder::from(31),
            17,
            PlaylistUpdateState::Partial,
            "provider",
            "completed",
        )
        .with_input_telemetry(telemetry.clone());

        let encoded = serde_json::to_string(&event)?;
        let decoded = serde_json::from_str::<PlaylistUpdateProgressEvent>(&encoded)?;

        assert_eq!(decoded.input_telemetry, Some(telemetry));
        assert_eq!(decoded, event);
        Ok(())
    }
}
