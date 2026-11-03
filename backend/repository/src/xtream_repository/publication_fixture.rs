//! Controlled real disk publication for dependent crates' integration tests.
use super::{
    get_collection_path, persist_input_xtream_playlist_clusters_to_disk, xtream_cluster_category_collection,
    XtreamClusterPublishBatchResult, XtreamClusterQualityPolicy, XtreamClusterRefreshRequest,
};
use shared::model::{UpdateQualityPolicy, XtreamCluster};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tuliprox_core::{
    model::{AppConfig, ConfigInput},
    utils::request::DynReader,
};

fn reader(value: serde_json::Value) -> DynReader {
    let (mut writer, reader) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        writer.write_all(value.to_string().as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
    });
    Box::pin(reader)
}

fn request(
    cluster: XtreamCluster,
    quality: XtreamClusterQualityPolicy,
    count: usize,
    generation: u32,
) -> XtreamClusterRefreshRequest {
    let category = match cluster {
        XtreamCluster::Live => 1,
        XtreamCluster::Video => 2,
        XtreamCluster::Series => 3,
    };
    let rows = (0..count).map(|i| {
        let id = generation + u32::try_from(i).unwrap();
        match cluster {
            XtreamCluster::Series=>serde_json::json!({"name":format!("series-{id}"),"series_id":id,"category_id":category}),
            _=>serde_json::json!({"name":format!("stream-{id}"),"stream_id":id,"category_id":category,"container_extension":"mp4"}),
        }
    }).collect::<Vec<_>>();
    XtreamClusterRefreshRequest {
        cluster,
        quality,
        categories: reader(serde_json::json!([{"category_id":category,"category_name":"fixture"}])),
        streams: reader(serde_json::json!(rows)),
    }
}

/// Seed actual `BTree`s, then publish a fresh ordered batch. A directory in place
/// of one active categories file causes a real publish-stage I/O failure.
/// The staged candidates still evaluate successfully before that later failure.
///
/// # Panics
/// Panics if fixture I/O or a prerequisite publication fails.
pub async fn publish(
    config: &Arc<AppConfig>,
    input: &ConfigInput,
    requested: &[XtreamCluster],
    failure: Option<XtreamCluster>,
    policy: UpdateQualityPolicy,
) -> XtreamClusterPublishBatchResult {
    let seed = requested
        .iter()
        .map(|cluster| request(*cluster, XtreamClusterQualityPolicy::Enforce { threshold: 0 }, 100, 1_000))
        .collect();
    let seeded = persist_input_xtream_playlist_clusters_to_disk(config, input, seed).await;
    assert!(seeded.errors.is_empty(), "seed publication must succeed");
    assert_eq!(seeded.completed_clusters, requested);
    if let Some(cluster) = failure {
        let storage = crate::get_input_storage_path(&input.name, &config.config.load().storage_dir).await.unwrap();
        let path = get_collection_path(&storage, xtream_cluster_category_collection(cluster));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
    }
    let requests = requested
        .iter()
        .map(|cluster| {
            let threshold = input.options.as_ref().map_or(0, |options| options.update_quality.threshold(*cluster));
            let quality = if policy == UpdateQualityPolicy::Bypass {
                XtreamClusterQualityPolicy::Bypass { configured_threshold: threshold }
            } else {
                XtreamClusterQualityPolicy::Enforce { threshold }
            };
            request(*cluster, quality, 100, 10_000)
        })
        .collect();
    persist_input_xtream_playlist_clusters_to_disk(config, input, requests).await
}
