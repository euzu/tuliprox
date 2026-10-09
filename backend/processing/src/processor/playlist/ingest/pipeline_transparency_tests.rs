use super::{
    telemetry::{finalize_input_telemetry, input_telemetry_for_fetch, persist_finalized_cluster_snapshots},
    *,
};
use shared::model::{
    ConfigInputOptionsDto, ConfigInputUpdateQualityDto, PersistedPlaylistUpdateClusterSnapshot,
    PersistedPlaylistUpdateQualityDecision, PersistedPlaylistUpdateQualitySnapshot,
    PersistedPlaylistUpdateTechnicalState, PlaylistUpdateClusterDecision, PlaylistUpdateClusterTelemetry,
    PlaylistUpdateDataSource,
};
use tuliprox_core::model::{ClusterUpdateAcceptance, ConfigInput};
use tuliprox_iptv::provider::PlaylistFetch;

fn input(input_type: InputType) -> ConfigInput {
    ConfigInput { id: 17, name: Arc::from("provider-a"), input_type, enabled: true, ..ConfigInput::default() }
}

fn cluster(telemetry: &PlaylistUpdateInputTelemetry, cluster: XtreamCluster) -> &PlaylistUpdateClusterTelemetry {
    telemetry.clusters.iter().find(|item| item.cluster == cluster).expect("configured cluster telemetry")
}

const fn completed_snapshot_context(
    effective_policy: InputRefreshPolicy,
    request_policy: Option<InputRefreshPolicy>,
    provider_cluster_count: usize,
) -> PersistedSnapshotContext {
    PersistedSnapshotContext {
        effective_policy,
        request_policy,
        technical_failure: false,
        storage_failure: false,
        provider_failure_evidence: ProviderFailureEvidence::None,
        provider_cluster_count,
    }
}

const fn failed_provider_snapshot_context(
    effective_policy: InputRefreshPolicy,
    request_policy: Option<InputRefreshPolicy>,
    provider_cluster_count: usize,
) -> PersistedSnapshotContext {
    PersistedSnapshotContext {
        effective_policy,
        request_policy,
        technical_failure: true,
        storage_failure: false,
        provider_failure_evidence: ProviderFailureEvidence::TechnicalFailure,
        provider_cluster_count,
    }
}

mod playlist;
mod policy;
mod storage;
mod streaming;
mod transport;
