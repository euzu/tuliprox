use super::{
    get_collection_path, persist_input_xtream_playlist_cluster_to_disk,
    preserve_details_with_injected_operation_failure, xtream_cluster_category_collection, DetailPreservationOperation,
    PreserveDetailsOutcome, XtreamClusterPublishOutcome,
};
use crate::BPlusTreeQuery;
use shared::{
    error::TuliproxError,
    model::{
        InputType, LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        SeriesStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{
    fs, io,
    path::Path,
    process::{Child, ExitStatus},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tuliprox_core::{
    model::{AppConfig, ConfigInput},
    utils::request::DynReader,
};

pub(in crate::xtream_repository::tests) fn json_reader(content: &str) -> DynReader {
    let (mut writer, reader) = tokio::io::duplex(4096);
    let content = content.as_bytes().to_vec();
    tokio::spawn(async move {
        writer.write_all(&content).await.expect("fixture should fit into duplex reader");
        writer.shutdown().await.expect("fixture writer should shut down");
    });
    Box::pin(reader)
}

pub(in crate::xtream_repository::tests) fn disk_test_input(name: &str) -> ConfigInput {
    ConfigInput {
        name: name.intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("user".to_string()),
        password: Some("password".to_string()),
        ..ConfigInput::default()
    }
}

pub(in crate::xtream_repository::tests) fn cluster_fixture_readers(
    cluster: XtreamCluster,
    category_name: &str,
    count: usize,
    first_provider_id: u32,
) -> (DynReader, DynReader) {
    let category_id = match cluster {
        XtreamCluster::Live => 1_u32,
        XtreamCluster::Video => 2,
        XtreamCluster::Series => 3,
    };
    let categories = serde_json::json!([{
        "category_id": category_id,
        "category_name": category_name,
    }])
    .to_string();
    let streams = (0..count)
        .map(|offset| {
            let provider_id =
                first_provider_id + u32::try_from(offset).expect("fixture item count should fit into u32");
            let name = format!("{category_name}-{provider_id}");
            match cluster {
                XtreamCluster::Live => serde_json::json!({
                    "name": name,
                    "stream_id": provider_id,
                    "category_id": category_id,
                    "added": "0",
                }),
                XtreamCluster::Video => serde_json::json!({
                    "name": name,
                    "stream_id": provider_id,
                    "category_id": category_id,
                    "added": "0",
                    "container_extension": "mp4",
                }),
                XtreamCluster::Series => serde_json::json!({
                    "name": name,
                    "series_id": provider_id,
                    "category_id": category_id,
                    "last_modified": "0",
                }),
            }
        })
        .collect::<Vec<_>>();
    let streams = serde_json::Value::Array(streams).to_string();
    (json_reader(&categories), json_reader(&streams))
}

pub(in crate::xtream_repository::tests) fn live_fixture_stream(
    provider_id: u32,
    category_id: u32,
    name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "stream_id": provider_id,
        "category_id": category_id,
        "added": "0",
    })
}

pub(in crate::xtream_repository::tests) async fn publish_live_fixture_rows(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    threshold: u8,
    categories: serde_json::Value,
    streams: Vec<serde_json::Value>,
) -> XtreamClusterPublishOutcome {
    persist_input_xtream_playlist_cluster_to_disk(
        app_config,
        input,
        XtreamCluster::Live,
        threshold,
        json_reader(&categories.to_string()),
        json_reader(&serde_json::Value::Array(streams).to_string()),
    )
    .await
    .expect("Live fixture refresh should complete")
}

pub(in crate::xtream_repository::tests) async fn publish_test_cluster(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster: XtreamCluster,
    threshold: u8,
    category_name: &str,
    count: usize,
    first_provider_id: u32,
) -> XtreamClusterPublishOutcome {
    let (categories, streams) = cluster_fixture_readers(cluster, category_name, count, first_provider_id);
    persist_input_xtream_playlist_cluster_to_disk(app_config, input, cluster, threshold, categories, streams)
        .await
        .expect("test cluster refresh should complete")
}

pub(in crate::xtream_repository::tests) fn active_cluster_count(storage_path: &Path, cluster: XtreamCluster) -> usize {
    super::super::count_xtream_tree_entries(&super::super::xtream_get_file_path(storage_path, cluster))
        .expect("active cluster should be countable")
        .expect("active cluster should exist")
}

pub(in crate::xtream_repository::tests) fn active_category_bytes(
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Vec<u8> {
    fs::read(get_collection_path(storage_path, xtream_cluster_category_collection(cluster)))
        .expect("active categories should be readable")
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::xtream_repository::tests) struct ActiveClusterSnapshot {
    pub(in crate::xtream_repository::tests) database: Vec<u8>,
    pub(in crate::xtream_repository::tests) categories: Vec<u8>,
}

pub(in crate::xtream_repository::tests) fn active_cluster_snapshot(
    storage_path: &Path,
    cluster: XtreamCluster,
) -> ActiveClusterSnapshot {
    let database_path = super::super::xtream_get_file_path(storage_path, cluster);
    ActiveClusterSnapshot {
        database: fs::read(&database_path).expect("active database should be readable"),
        categories: active_category_bytes(storage_path, cluster),
    }
}

pub(in crate::xtream_repository::tests) fn fail_detail_preservation_after_quality(
    published_path: &Path,
    staging_path: &Path,
) -> Result<PreserveDetailsOutcome, TuliproxError> {
    preserve_details_with_injected_operation_failure(published_path, staging_path, DetailPreservationOperation::Commit)
}

pub(in crate::xtream_repository::tests) fn read_series_props(path: &Path, provider_id: u32) -> SeriesStreamProperties {
    let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(path).expect("query open should succeed");
    let item = query.query_zero_copy(&provider_id).expect("query should succeed").expect("item should exist");
    match item.additional_properties {
        Some(StreamProperties::Series(series)) => *series,
        other => panic!("expected series stream properties, got {other:?}"),
    }
}

pub(in crate::xtream_repository::tests) fn assert_no_refresh_artifacts(storage_path: &Path) {
    let entries = fs::read_dir(storage_path)
        .expect("input storage should be readable")
        .collect::<io::Result<Vec<_>>>()
        .expect("input storage entries should be readable");
    let refresh_artifacts = entries
        .into_iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("refresh-"))
        .collect::<Vec<_>>();
    assert!(refresh_artifacts.is_empty(), "staging artifacts survived: {refresh_artifacts:?}");
}

pub(in crate::xtream_repository::tests) fn wait_for_child(
    mut child: Child,
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "Xtream refresh child timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

pub(in crate::xtream_repository::tests) fn make_input_group(
    cluster: XtreamCluster,
    category_id: u32,
    category_name: &str,
    provider_id: u32,
) -> PlaylistGroup {
    let stream_id = provider_id.to_string().intern();
    let category_name = category_name.intern();
    PlaylistGroup {
        id: category_id,
        title: Arc::clone(&category_name),
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                id: Arc::clone(&stream_id),
                input_stream_id: stream_id,
                name: format!("stream-{provider_id}").intern(),
                title: format!("stream-{provider_id}").intern(),
                group: category_name,
                url: format!("http://provider.example/{cluster}/{provider_id}").intern(),
                item_type: PlaylistItemType::from(cluster),
                xtream_cluster: cluster,
                category_id,
                input_name: "provider-a".intern(),
                ..PlaylistItemHeader::default()
            },
        }],
        xtream_cluster: cluster,
    }
}

pub(in crate::xtream_repository::tests) fn read_live_props(path: &Path, provider_id: u32) -> LiveStreamProperties {
    let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(path).expect("query open should succeed");
    let item = query.query_zero_copy(&provider_id).expect("query should succeed").expect("item should exist");
    match item.additional_properties {
        Some(StreamProperties::Live(live)) => *live,
        other => panic!("expected live stream properties, got {other:?}"),
    }
}
