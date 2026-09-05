use crate::input_cache::{self, ClusterState, InputStatus};
use log::warn;
use shared::model::{EventMessage, EventSink, PlaylistUpdateProgressEvent, XtreamCluster};
use tuliprox_core::model::{ClusterUpdateRejection, ConfigInput};
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
    input: &ConfigInput,
    status: &mut InputStatus,
    cache_scope: CacheStatusScope<'_>,
    fetch: &PlaylistFetch,
) -> bool {
    report_quality_rejections(events, input, &fetch.quality_rejections);

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
            let mut changed = false;
            if technical_failure {
                input_cache::update_cluster_status(status, "default", ClusterState::Failed);
                changed = true;
            } else if fetch.quality_rejections.is_empty() {
                input_cache::update_cluster_status(status, "default", ClusterState::Ok);
                changed = true;
            }

            for rejection in &fetch.quality_rejections {
                input_cache::update_cluster_status(status, rejection.cluster.as_ref(), ClusterState::Failed);
                changed = true;
            }
            changed
        }
    }
}

fn report_quality_rejections<E: EventSink>(events: &E, input: &ConfigInput, rejections: &[ClusterUpdateRejection]) {
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
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
            target: input.name.to_string(),
            message,
        }));
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
    use crate::input_cache::{ClusterState, ClusterStatus, InputStatus};
    use shared::model::{EventMessage, EventSink, InputType, XtreamCluster};
    use std::sync::{Arc, Mutex};
    use tuliprox_core::model::{ClusterUpdateRejection, ConfigInput};
    use tuliprox_iptv::provider::PlaylistFetch;

    #[derive(Clone, Default)]
    struct CollectSink(Arc<Mutex<Vec<EventMessage>>>);

    impl EventSink for CollectSink {
        fn emit(&self, event: EventMessage) { self.0.lock().expect("event sink lock").push(event); }
    }

    fn rejection(cluster: XtreamCluster) -> ClusterUpdateRejection {
        ClusterUpdateRejection { cluster, current_count: 12_543, candidate_count: 217, threshold: 90, quality: 1 }
    }

    #[test]
    fn synthetic_mixed_cluster_fetch_updates_status_and_reports_rejection() {
        let input = ConfigInput { name: Arc::from("provider-a"), input_type: InputType::Xtream, ..Default::default() };
        let events = CollectSink::default();
        let mut status = InputStatus::default();
        status.clusters.insert("series".to_string(), ClusterStatus { status: ClusterState::Ok, timestamp: 17 });
        let fetch = PlaylistFetch::groups(Vec::new()).with_quality_rejections(vec![rejection(XtreamCluster::Video)]);

        let changed = apply_playlist_fetch_outcome(
            &events,
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
        assert_eq!(progress.target, "provider-a");
        assert_eq!(
            progress.message,
            "Input 'provider-a' cluster 'vod' rejected: current=12543 candidate=217 threshold=90 quality=1; retaining previous cluster"
        );
    }

    #[test]
    fn synthetic_default_fetch_does_not_refresh_default_cache_after_rejection() {
        let input = ConfigInput { name: Arc::from("provider-a"), ..Default::default() };
        let events = CollectSink::default();
        let mut status = InputStatus::default();
        status.clusters.insert("default".to_string(), ClusterStatus { status: ClusterState::Ok, timestamp: 23 });
        let fetch = PlaylistFetch::groups(Vec::new()).with_quality_rejections(vec![rejection(XtreamCluster::Series)]);

        let changed = apply_playlist_fetch_outcome(&events, &input, &mut status, CacheStatusScope::Default, &fetch);

        assert!(changed);
        let default = status.clusters.get("default").expect("default cache status");
        assert_eq!(default.status, ClusterState::Ok);
        assert_eq!(default.timestamp, 23);
        assert_eq!(
            status.clusters.get(XtreamCluster::Series.as_ref()).map(|entry| &entry.status),
            Some(&ClusterState::Failed)
        );
    }
}
