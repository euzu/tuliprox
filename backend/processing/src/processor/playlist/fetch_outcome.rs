use crate::input_cache::{self, ClusterState, InputStatus};
use log::warn;
use shared::model::{
    EventMessage, EventSink, PlaylistUpdateProgressEvent, PlaylistUpdateRunId, PlaylistUpdateRunOrder, XtreamCluster,
};
use tuliprox_core::model::{ClusterForceUpdate, ClusterUpdateRejection, ConfigInput};
use tuliprox_iptv::provider::PlaylistFetch;

/// Cache entries whose state is determined by one completed provider fetch.
#[derive(Clone, Copy)]
pub(super) enum CacheStatusScope<'a> {
    Default,
    RequestedClusters(&'a [XtreamCluster]),
}

/// Applies the observable and cache-state effects of a completed provider fetch.
///
/// Quality rejections remain separate from technical errors. For per-cluster
/// fetches, only requested clusters are updated, so valid cached clusters keep
/// both their state and timestamp.
pub(super) fn apply_playlist_fetch_outcome<E: EventSink>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    input: &ConfigInput,
    status: &mut InputStatus,
    cache_scope: CacheStatusScope<'_>,
    fetch: &PlaylistFetch,
) -> bool {
    report_quality_rejections(events, run_id, execution_order, input, &fetch.quality_rejections);
    if fetch.persisted {
        report_force_updates(events, run_id, execution_order, input, &fetch.force_updates);
    }

    let technical_failure = fetch.partial || !fetch.errors.is_empty();
    match cache_scope {
        CacheStatusScope::RequestedClusters(requested_clusters) => {
            for cluster in requested_clusters {
                let rejected = fetch.quality_rejections.iter().any(|rejection| rejection.cluster == *cluster);
                let state = if technical_failure || rejected { ClusterState::Failed } else { ClusterState::Ok };
                input_cache::update_cluster_status(status, cluster.as_ref(), state);
            }
            !requested_clusters.is_empty()
        }
        CacheStatusScope::Default => {
            let default_state = if technical_failure || !fetch.quality_rejections.is_empty() {
                ClusterState::Failed
            } else {
                ClusterState::Ok
            };
            input_cache::update_cluster_status(status, "default", default_state);

            // M3U still acquires/caches one document, but its completed decisions
            // and reload snapshots are per cluster. A rejected cluster invalidates
            // the document cache without labelling accepted siblings Failed.
            if input.get_download_input_type() == shared::model::InputType::M3u {
                for cluster in super::ingest::PIPELINE_TRANSPARENCY_CLUSTERS {
                    if super::ingest::cluster_is_configured(input, cluster) {
                        let rejected = fetch.quality_rejections.iter().any(|item| item.cluster == cluster);
                        let state = if technical_failure || rejected { ClusterState::Failed } else { ClusterState::Ok };
                        input_cache::update_cluster_status(status, cluster.as_ref(), state);
                    }
                }
            }

            for rejection in &fetch.quality_rejections {
                input_cache::update_cluster_status(status, rejection.cluster.as_ref(), ClusterState::Failed);
            }
            true
        }
    }
}

pub(super) fn report_force_updates<E: EventSink>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    input: &ConfigInput,
    force_updates: &[ClusterForceUpdate],
) {
    for force_update in force_updates {
        let message = format!(
            "Input '{}' cluster '{}' force-published: candidate={} configured_threshold={}",
            input.name,
            quality_cluster_name(force_update.cluster),
            force_update.candidate_count,
            force_update.configured_threshold
        );
        log::info!("{message}");
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
            run_id.clone(),
            execution_order,
            input.id,
            input.name.to_string(),
            message,
        )));
    }
}

fn report_quality_rejections<E: EventSink>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    input: &ConfigInput,
    rejections: &[ClusterUpdateRejection],
) {
    for rejection in rejections {
        let message = format!(
            "Input '{}' cluster '{}' rejected: current={} candidate={} threshold={} quality={}; retaining previous cluster",
            input.name,
            quality_cluster_name(rejection.cluster),
            rejection.current_count,
            rejection.candidate_count,
            rejection.threshold,
            rejection.quality
        );
        warn!("{message}");
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
            run_id.clone(),
            execution_order,
            input.id,
            input.name.to_string(),
            message,
        )));
    }
}

const fn quality_cluster_name(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => "live",
        XtreamCluster::Video => "vod",
        XtreamCluster::Series => "series",
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_playlist_fetch_outcome, CacheStatusScope};
    use crate::input_cache::{is_cache_valid, ClusterState, ClusterStatus, InputStatus};
    use shared::model::{
        EventMessage, EventSink, InputType, PlaylistUpdateRunId, PlaylistUpdateRunOrder, XtreamCluster,
    };
    use std::sync::{Arc, Mutex};
    use tuliprox_core::model::{ClusterForceUpdate, ClusterUpdateRejection, ConfigInput};
    use tuliprox_iptv::provider::PlaylistFetch;

    #[derive(Clone, Default)]
    struct CollectSink(Arc<Mutex<Vec<EventMessage>>>);

    impl EventSink for CollectSink {
        fn emit(&self, event: EventMessage) { self.0.lock().expect("event sink lock").push(event); }
    }

    fn rejection(cluster: XtreamCluster) -> ClusterUpdateRejection {
        ClusterUpdateRejection { cluster, current_count: 12_543, candidate_count: 217, threshold: 90, quality: 1 }
    }

    fn run_id() -> PlaylistUpdateRunId { "fetch-outcome-run".into() }

    fn execution_order() -> PlaylistUpdateRunOrder { PlaylistUpdateRunOrder::from(41) }

    #[test]
    fn playlist_update_run_mixed_cluster_fetch_reports_correlated_rejection() {
        let input = ConfigInput { name: Arc::from("provider-a"), input_type: InputType::Xtream, ..Default::default() };
        let events = CollectSink::default();
        let mut status = InputStatus::default();
        status
            .clusters
            .insert("series".to_string(), ClusterStatus { status: ClusterState::Ok, timestamp: 17, last_update: None });
        let fetch = PlaylistFetch::groups(Vec::new()).with_quality_rejections(vec![rejection(XtreamCluster::Video)]);

        let changed = apply_playlist_fetch_outcome(
            &events,
            &run_id(),
            execution_order(),
            &input,
            &mut status,
            CacheStatusScope::RequestedClusters(&[XtreamCluster::Live, XtreamCluster::Video]),
            &fetch,
        );

        assert!(changed);
        assert_eq!(
            status.clusters.get(XtreamCluster::Live.as_ref()).map(|entry| &entry.status),
            Some(&ClusterState::Ok)
        );
        assert_eq!(
            status.clusters.get(XtreamCluster::Video.as_ref()).map(|entry| &entry.status),
            Some(&ClusterState::Failed)
        );
        let series = status.clusters.get("series").expect("cached series status");
        assert_eq!(series.status, ClusterState::Ok);
        assert_eq!(series.timestamp, 17);
        assert!(!status.clusters.contains_key("default"));

        let emitted = events.0.lock().expect("event sink lock");
        assert_eq!(emitted.len(), 1);
        let EventMessage::PlaylistUpdateProgress(progress) = &emitted[0] else {
            panic!("expected playlist update progress event");
        };
        assert_eq!(progress.run_id.as_ref().map(AsRef::as_ref), Some("fetch-outcome-run"));
        assert_eq!(progress.execution_order, Some(execution_order()));
        assert_eq!(progress.target, "provider-a");
        assert_eq!(
            progress.message,
            "Input 'provider-a' cluster 'vod' rejected: current=12543 candidate=217 threshold=90 quality=1; retaining previous cluster"
        );
    }

    #[test]
    fn synthetic_default_fetch_invalidates_default_and_cluster_cache_after_rejection() {
        let input = ConfigInput { name: Arc::from("provider-a"), input_type: InputType::Stalker, ..Default::default() };
        let events = CollectSink::default();
        let mut status = InputStatus::default();
        status.clusters.insert(
            "default".to_string(),
            ClusterStatus { status: ClusterState::Ok, timestamp: 23, last_update: None },
        );
        let fetch = PlaylistFetch::groups(Vec::new()).with_quality_rejections(vec![rejection(XtreamCluster::Series)]);

        let changed = apply_playlist_fetch_outcome(
            &events,
            &run_id(),
            execution_order(),
            &input,
            &mut status,
            CacheStatusScope::Default,
            &fetch,
        );

        assert!(changed);
        let default = status.clusters.get("default").expect("default cache status");
        assert_eq!(default.status, ClusterState::Failed);
        assert!(!is_cache_valid(&status, "default", u64::MAX));
        assert_eq!(
            status.clusters.get(XtreamCluster::Series.as_ref()).map(|entry| &entry.status),
            Some(&ClusterState::Failed)
        );
    }

    #[test]
    fn playlist_update_run_force_publication_emits_correlated_audit_progress() {
        let input = ConfigInput { name: Arc::from("provider-a"), input_type: InputType::Xtream, ..Default::default() };
        let events = CollectSink::default();
        let mut status = InputStatus::default();
        status.clusters.insert(
            XtreamCluster::Video.as_ref().to_string(),
            ClusterStatus { status: ClusterState::Failed, timestamp: 17, last_update: None },
        );
        let fetch = PlaylistFetch::default()
            .with_force_updates(vec![ClusterForceUpdate {
                cluster: XtreamCluster::Video,
                candidate_count: 12_450,
                configured_threshold: 95,
            }])
            .persisted(true);

        let changed = apply_playlist_fetch_outcome(
            &events,
            &run_id(),
            execution_order(),
            &input,
            &mut status,
            CacheStatusScope::RequestedClusters(&[XtreamCluster::Video]),
            &fetch,
        );

        assert!(changed);
        assert_eq!(
            status.clusters.get(XtreamCluster::Video.as_ref()).map(|entry| &entry.status),
            Some(&ClusterState::Ok)
        );
        let emitted = events.0.lock().expect("event sink lock");
        assert_eq!(emitted.len(), 1);
        let EventMessage::PlaylistUpdateProgress(progress) = &emitted[0] else {
            panic!("expected playlist update progress event");
        };
        assert_eq!(progress.run_id.as_ref().map(AsRef::as_ref), Some("fetch-outcome-run"));
        assert_eq!(progress.execution_order, Some(execution_order()));
        assert_eq!(progress.target, "provider-a");
        assert_eq!(
            progress.message,
            "Input 'provider-a' cluster 'vod' force-published: candidate=12450 configured_threshold=95"
        );
    }
}
