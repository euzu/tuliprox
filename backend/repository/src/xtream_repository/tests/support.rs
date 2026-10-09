use super::XtreamRefreshPaths;
use crate::BPlusTreeUpdate;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    model::{
        ConfigPaths, LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        ProcessingOrder, StreamProperties, VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{path::Path, sync::Arc};
use tuliprox_core::{
    model::{
        ApiProxyConfig, AppConfig, Config, ConfigTarget, CustomStreamResponse, HdHomeRunConfig, MediaToolCapabilities,
        SourcesConfig, StagedFilter, TargetExecutionPlan, TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    utils::FileLockManager,
};
use uuid::Uuid;

pub(in crate::xtream_repository::tests) fn test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config {
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            ..Config::default()
        })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::<HdHomeRunConfig>::default()),
        api_proxy: Arc::new(ArcSwapOption::<ApiProxyConfig>::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::<CustomStreamResponse>::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    })
}

pub(in crate::xtream_repository::tests) fn target_writer_config() -> ConfigTarget {
    ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: "target-empty-cluster-guard".to_string(),
        options: None,
        sort: None,
        filter: StagedFilter::default(),
        output: vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    }
}

pub(in crate::xtream_repository::tests) fn target_writer_group(
    cluster: XtreamCluster,
    category_id: u32,
    virtual_id: u32,
) -> PlaylistGroup {
    let title = format!("target-{cluster}").intern();
    PlaylistGroup {
        id: category_id,
        title: Arc::clone(&title),
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                id: virtual_id.to_string().intern(),
                input_stream_id: virtual_id.to_string().intern(),
                virtual_id: VirtualId::new(virtual_id),
                name: Arc::clone(&title),
                title: Arc::clone(&title),
                group: title,
                input_name: "target-writer-input".intern(),
                item_type: PlaylistItemType::from(cluster),
                xtream_cluster: cluster,
                category_id,
                ..PlaylistItemHeader::default()
            },
        }],
        xtream_cluster: cluster,
    }
}

pub(in crate::xtream_repository::tests) fn make_live_item(
    provider_id: u32,
    video: Option<&str>,
    audio: Option<&str>,
    last_probed_timestamp: Option<i64>,
    last_success_timestamp: Option<i64>,
    bitrate: u32,
) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(provider_id),
        provider_id,
        name: "Live".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "group".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.com/live.ts".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
            video: video.map(Internable::intern),
            audio: audio.map(Internable::intern),
            last_probed_timestamp,
            last_success_timestamp,
            bitrate,
            ..Default::default()
        }))),
        item_type: shared::model::PlaylistItemType::Live,
        category_id: 1,
        input_name: "input_a".intern(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: provider_id.to_string().intern(),
        upstream_user_agent: None,
    }
}

pub(in crate::xtream_repository::tests) fn write_single_item(path: &Path, item: &XtreamPlaylistItem) {
    crate::BPlusTree::<u32, XtreamPlaylistItem>::new().store(path).expect("tree creation should succeed");
    let mut tree =
        BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(path).expect("tree open should succeed");
    let batch: Vec<(&u32, &XtreamPlaylistItem)> = vec![(&item.provider_id, item)];
    let prepared = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch)
        .expect("batch preparation should succeed");
    tree.upsert_batch_encoded(prepared).expect("batch upsert should succeed");
    tree.commit().expect("tree commit should succeed");
}

pub(in crate::xtream_repository::tests) fn fixed_refresh_paths(path: &Path, generation: u128) -> XtreamRefreshPaths {
    XtreamRefreshPaths::for_generation(path, XtreamCluster::Live, Uuid::from_u128(generation))
        .expect("fixed refresh paths should be valid")
}

pub(in crate::xtream_repository::tests) fn write_detail_preservation_fixture(
    paths: &XtreamRefreshPaths,
    provider_id: u32,
) {
    write_single_item(
        &paths.published_database,
        &make_live_item(
            provider_id,
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some(1_700_000_000),
            Some(1_700_000_100),
            2_500_000,
        ),
    );
    write_single_item(&paths.staging_database, &make_live_item(provider_id, None, None, None, None, 0));
}
