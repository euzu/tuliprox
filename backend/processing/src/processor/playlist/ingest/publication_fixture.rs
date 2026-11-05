//! Cross-crate integration fixture using the real disk publisher and status writer.
use super::*;
use shared::model::{EventMessage, EventSink, PlaylistUpdateDataSource};
use std::sync::Mutex as StdMutex;
use tuliprox_core::model::ConfigInput;

#[derive(Default)]
struct Capture(StdMutex<Vec<EventMessage>>);
impl EventSink for Capture {
    fn emit(&self, event: EventMessage) { self.0.lock().unwrap().push(event); }
}

/// Runs real publication -> provider adapter -> telemetry -> canonical file and
/// completion event. The injected failure is an actual category-publish I/O error.
///
/// # Panics
/// Panics if fixture I/O, publication prerequisites or boundary assertions fail.
pub async fn publication_fixture_status(
    config: &Arc<AppConfig>,
    input: &ConfigInput,
    requested: &[XtreamCluster],
    failure: Option<XtreamCluster>,
    policy: InputRefreshPolicy,
) -> (PlaylistUpdateProgressEvent, input_cache::InputStatus) {
    let fetch =
        tuliprox_iptv::xtream::publication_fixture_fetch(config, input, requested, failure, policy.quality).await;
    let expected: Vec<_> = requested.iter().copied().take_while(|cluster| Some(*cluster) != failure).collect();
    assert_eq!(fetch.completed_clusters, expected, "only actual completed publications cross the provider boundary");
    for cluster in &expected {
        assert_eq!(tuliprox_repository::count_input_xtream_cluster(config, input, *cluster).await.unwrap(), Some(100));
    }
    let path = input_cache::resolve_input_storage_path(&config.config.load().storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&path);
    let events = Capture::default();
    let run_id = "publication-fixture-R2".into();
    let order = 42.into();
    apply_playlist_fetch_outcome(
        &events,
        &run_id,
        order,
        input,
        &mut status,
        CacheStatusScope::RequestedClusters(requested),
        &fetch,
    );
    let telemetry = telemetry::input_telemetry_for_fetch(
        input,
        policy,
        PlaylistUpdateDataSource::Provider,
        requested,
        Some(&fetch),
    );
    let state = if fetch.errors.is_empty() { PlaylistUpdateState::Success } else { PlaylistUpdateState::Failure };
    let mut result = PlaylistDownloadResult::from(fetch).with_input_telemetry(telemetry).with_pending_status(
        PendingInputStatusUpdate {
            storage_path: path.clone(),
            status,
            status_changed: true,
            cluster_status_source: ClusterStatusSource::PerCluster,
        },
    );
    telemetry::finalize_input_telemetry(&mut result, None);
    telemetry::persist_finalized_cluster_snapshots(&mut result, Some(policy), None).await;
    let ctx = PlaylistProcessingContext::for_epg_test(config.clone(), reqwest::Client::new());
    super::super::input_status::persist_input_completion(&ctx, input.id, &input.name, state).await;
    report_input_completion(&events, &run_id, order, input.id, &input.name, state, result.input_telemetry.as_ref());
    let event = events
        .0
        .into_inner()
        .unwrap()
        .into_iter()
        .filter_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) if progress.state.is_some() => Some(progress),
            _ => None,
        })
        .next_back()
        .unwrap();
    let saved = input_cache::load_input_status(&path);
    for cluster in
        event.input_telemetry.as_ref().unwrap().clusters.iter().filter(|cluster| requested.contains(&cluster.cluster))
    {
        if let Some(snapshot) = saved.clusters.get(cluster.cluster.as_ref()).and_then(|s| s.last_update) {
            assert_eq!(snapshot.technical_state, cluster.technical_state);
            assert_eq!(snapshot.source, cluster.source);
        }
    }
    (event, saved)
}

/// Read the actually published files again, then finalize an ordinary cache hit.
///
/// # Panics
/// Panics if the real cache read or preservation assertions fail.
pub async fn cache_fixture_status(
    config: &Arc<AppConfig>,
    input: &ConfigInput,
) -> (PlaylistUpdateProgressEvent, input_cache::InputStatus) {
    let path = input_cache::resolve_input_storage_path(&config.config.load().storage_dir, &input.name).await;
    let previous = input_cache::load_input_status(&path);
    let mut loaded = tuliprox_repository::load_input_playlist(config, input, None).await.expect("real cache read");
    assert!(!loaded.is_empty());
    let mut result = status::cached_playlist_download_result(input, InputRefreshPolicy::NORMAL).with_pending_status(
        PendingInputStatusUpdate {
            storage_path: path.clone(),
            status: previous.clone(),
            status_changed: false,
            cluster_status_source: ClusterStatusSource::PerCluster,
        },
    );
    telemetry::finalize_input_telemetry(&mut result, None);
    telemetry::persist_finalized_cluster_snapshots(&mut result, None, None).await;
    let events = Capture::default();
    report_input_completion(
        &events,
        &"publication-fixture-R2".into(),
        42.into(),
        input.id,
        &input.name,
        PlaylistUpdateState::Success,
        result.input_telemetry.as_ref(),
    );
    let event = events
        .0
        .into_inner()
        .unwrap()
        .into_iter()
        .find_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) => Some(progress),
            _ => None,
        })
        .unwrap();
    let saved = input_cache::load_input_status(&path);
    for (key, value) in &previous.clusters {
        assert_eq!(saved.clusters[key].timestamp, value.timestamp);
        assert_eq!(saved.clusters[key].status, value.status);
    }
    assert_eq!(previous.last_input_update, saved.last_input_update);
    assert_eq!(previous.epg_sources, saved.epg_sources);
    let ctx = PlaylistProcessingContext::for_epg_test(config.clone(), reqwest::Client::new());
    super::super::input_status::persist_input_completion(&ctx, input.id, &input.name, PlaylistUpdateState::Success)
        .await;
    (event, input_cache::load_input_status(&path))
}
