use crate::model::{
    InputRefreshPolicy, PlaylistUpdateDataSource, PlaylistUpdateProgressEvent, PlaylistUpdateState, XtreamCluster,
};
use serde::{Deserialize, Serialize};

/// Persisted outcome exposed by the canonical input-cache status read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedPlaylistUpdateClusterState {
    Ok,
    Failed,
}

/// Persisted result of an already completed Quality evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedPlaylistUpdateQualityDecision {
    Accepted,
    Rejected,
}

/// Persisted Quality facts copied from the typed runtime decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPlaylistUpdateQualitySnapshot {
    pub threshold: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achieved_quality: Option<u8>,
    pub decision: PersistedPlaylistUpdateQualityDecision,
}

/// Technical completion of the last cluster update, independent of Quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedPlaylistUpdateTechnicalState {
    Succeeded,
    Failed,
}

/// Exactly one last, non-historical runtime snapshot for a persisted cluster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPlaylistUpdateClusterSnapshot {
    /// Present only when a request-local policy was typed for this input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<InputRefreshPolicy>,
    /// Actual data source; never reconstructed from `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PlaylistUpdateDataSource>,
    /// Guard configured for this run: zero means disabled, a positive value is active.
    /// Missing only for legacy or deliberately neutralized snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_guard_threshold: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<PersistedPlaylistUpdateQualitySnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technical_state: Option<PersistedPlaylistUpdateTechnicalState>,
}

/// Read projection of one persisted Xtream-compatible cluster status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPlaylistUpdateClusterStatusDto {
    pub cluster: XtreamCluster,
    pub status: PersistedPlaylistUpdateClusterState,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<PersistedPlaylistUpdateClusterSnapshot>,
}

/// Last completed input job, not the result of the subsequent target publication.
/// Replaced on completion; independent of cache validity and cluster snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPlaylistUpdateInputResult {
    pub state: PlaylistUpdateState,
    pub timestamp: u64,
}

/// Persisted playlist-update facts for one configured input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputPlaylistUpdateStatusDto {
    pub input_id: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_update: Option<PersistedPlaylistUpdateInputResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<PersistedPlaylistUpdateClusterStatusDto>,
}

/// Persisted input results plus optional, volatile facts of still running updates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistUpdateStatusDto {
    #[serde(default)]
    pub inputs: Vec<InputPlaylistUpdateStatusDto>,
    /// Existing correlated progress facts, never written into status.json.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_updates: Vec<PlaylistUpdateProgressEvent>,
}

#[cfg(test)]
mod tests {
    use super::{
        InputPlaylistUpdateStatusDto, PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateClusterState,
        PersistedPlaylistUpdateClusterStatusDto, PersistedPlaylistUpdateQualityDecision,
        PersistedPlaylistUpdateQualitySnapshot, PersistedPlaylistUpdateTechnicalState, PlaylistUpdateStatusDto,
    };
    use crate::model::{InputRefreshPolicy, PlaylistUpdateDataSource, XtreamCluster};

    #[test]
    fn playlist_update_status_last_input_result_is_optional_terminal_and_round_trips() -> Result<(), serde_json::Error>
    {
        let legacy = serde_json::from_str::<InputPlaylistUpdateStatusDto>(r#"{"input_id":7,"last_update":23}"#)?;
        assert_eq!(legacy.last_input_update, None);
        for state in [
            super::PlaylistUpdateState::Success,
            super::PlaylistUpdateState::Partial,
            super::PlaylistUpdateState::Failure,
        ] {
            let result = super::PersistedPlaylistUpdateInputResult { state, timestamp: 123 };
            let dto = InputPlaylistUpdateStatusDto { last_input_update: Some(result), ..legacy.clone() };
            assert_eq!(serde_json::from_str::<InputPlaylistUpdateStatusDto>(&serde_json::to_string(&dto)?)?, dto);
        }
        // A transient queued/updating state must never become a durable terminal result.
        assert!(serde_json::from_str::<super::PersistedPlaylistUpdateInputResult>(
            r#"{"state":"Queued","timestamp":123}"#
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn playlist_update_status_dto_round_trips_ids_timestamps_and_cluster_states() -> Result<(), serde_json::Error> {
        let status = PlaylistUpdateStatusDto {
            inputs: vec![
                InputPlaylistUpdateStatusDto {
                    input_id: 7,
                    last_update: Some(1_725_000_000),
                    last_input_update: None,
                    clusters: vec![PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Video,
                        status: PersistedPlaylistUpdateClusterState::Failed,
                        timestamp: 1_725_000_000,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: Some(InputRefreshPolicy::REFRESH),
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: None,
                            quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                                threshold: 95,
                                baseline_count: Some(4_812),
                                candidate_count: Some(3_104),
                                achieved_quality: Some(64),
                                decision: PersistedPlaylistUpdateQualityDecision::Rejected,
                            }),
                            active_count: Some(4_812),
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    }],
                },
                InputPlaylistUpdateStatusDto {
                    input_id: 9,
                    last_update: None,
                    last_input_update: None,
                    clusters: Vec::new(),
                },
            ],
            ..PlaylistUpdateStatusDto::default()
        };

        let encoded = serde_json::to_string(&status)?;
        let decoded = serde_json::from_str::<PlaylistUpdateStatusDto>(&encoded)?;

        assert_eq!(decoded, status);
        Ok(())
    }

    #[test]
    fn playlist_update_status_dto_accepts_legacy_payload_without_cluster_statuses() -> Result<(), serde_json::Error> {
        let decoded =
            serde_json::from_str::<PlaylistUpdateStatusDto>(r#"{"inputs":[{"input_id":7,"last_update":1725000000}]}"#)?;

        assert!(decoded.inputs[0].clusters.is_empty());
        assert!(decoded.active_updates.is_empty());
        Ok(())
    }

    #[test]
    fn playlist_update_status_active_read_facts_round_trip_without_changing_persisted_results() {
        let event = super::PlaylistUpdateProgressEvent::for_run_input("r1".into(), 1.into(), 7, "provider", "updating");
        let status = PlaylistUpdateStatusDto { active_updates: vec![event], ..PlaylistUpdateStatusDto::default() };
        let encoded = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<PlaylistUpdateStatusDto>(&encoded).unwrap(), status);
        assert!(!serde_json::to_string(&PlaylistUpdateStatusDto::default()).unwrap().contains("active_updates"));
        assert!(serde_json::from_str::<super::PersistedPlaylistUpdateInputResult>(
            r#"{"state":"Updating","timestamp":123}"#
        )
        .is_err());
    }

    #[test]
    fn playlist_update_status_dto_accepts_block_22_cluster_without_snapshot() -> Result<(), serde_json::Error> {
        let decoded = serde_json::from_str::<PlaylistUpdateStatusDto>(
            r#"{"inputs":[{"input_id":7,"clusters":[{"cluster":"Live","status":"ok","timestamp":1725000000}]}]}"#,
        )?;

        assert_eq!(decoded.inputs[0].clusters[0].last_update, None);
        Ok(())
    }

    #[test]
    fn playlist_update_status_dto_accepts_block_22a_snapshot_without_historical_guard() -> Result<(), serde_json::Error>
    {
        let decoded = serde_json::from_str::<PlaylistUpdateStatusDto>(
            r#"{"inputs":[{"input_id":7,"clusters":[{"cluster":"Live","status":"ok","timestamp":1725000000,"last_update":{"policy":{"cache":"bypass","quality":"bypass"},"source":"provider","active_count":42,"technical_state":"succeeded"}}]}]}"#,
        )?;

        let snapshot = decoded.inputs[0].clusters[0].last_update.expect("Block-22a snapshot");
        assert_eq!(snapshot.policy, Some(InputRefreshPolicy::FORCE));
        assert_eq!(snapshot.quality_guard_threshold, None);
        Ok(())
    }
}
