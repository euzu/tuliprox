//! Shared `AppConfig` constructors for the repository crate's tests.

use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    model::{ConfigPaths, PlaylistItemType, ProcessingOrder, VirtualId, XtreamCluster, XtreamPlaylistItem},
    utils::Internable,
};
use std::{path::Path, sync::Arc};
use tuliprox_core::{
    model::{
        ApiProxyConfig, AppConfig, Config, ConfigTarget, MediaToolCapabilities, SourcesConfig, StagedFilter,
        TargetExecutionPlan, TargetOutput,
    },
    utils::FileLockManager,
};

/// `ConfigPaths` with every path empty; tests override the fields they care about.
pub(crate) fn empty_paths() -> ConfigPaths {
    ConfigPaths {
        home_path: String::new(),
        config_path: String::new(),
        storage_path: String::new(),
        config_file_path: String::new(),
        sources_file_path: String::new(),
        mapping_file_path: None,
        mapping_files_used: None,
        template_file_path: None,
        template_files_used: None,
        failover_file_path: None,
        api_proxy_file_path: String::new(),
        custom_stream_response_path: None,
    }
}

/// The single place that assembles a test `AppConfig`; every other constructor builds on it.
pub(crate) fn app_config_from(config: Config, paths: ConfigPaths, api_proxy: Option<ApiProxyConfig>) -> AppConfig {
    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::new(api_proxy.map(Arc::new))),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths)),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
        failover: Arc::default(),
    }
}

/// Default config, empty paths, no API proxy.
pub(crate) fn app_config() -> Arc<AppConfig> { Arc::new(app_config_from(Config::default(), empty_paths(), None)) }

/// Default config with `storage_dir` pointing at `storage_dir`, empty paths, no API proxy.
pub(crate) fn app_config_in(storage_dir: &Path) -> Arc<AppConfig> {
    let config = Config { storage_dir: storage_dir.to_string_lossy().into_owned(), ..Config::default() };
    Arc::new(app_config_from(config, empty_paths(), None))
}

/// Enabled target named `name` with a single `output` and every other option unset.
pub(crate) fn target_with_output(name: &str, output: TargetOutput) -> ConfigTarget {
    ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: name.to_string(),
        options: None,
        sort: None,
        filter: StagedFilter::default(),
        output: vec![output],
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

/// Live Xtream playlist item whose provider and virtual id are `id`.
pub(crate) fn xtream_playlist_item(id: u32) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(id),
        provider_id: id,
        name: "name".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "group".intern(),
        title: "title".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.test/live.ts".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: "input".intern(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: id.to_string().intern(),
        upstream_user_agent: None,
    }
}

/// Series Xtream playlist item whose provider and virtual id are `id`.
pub(crate) fn xtream_series_item(
    id: u32,
    details: Option<shared::model::SeriesStreamDetailProperties>,
) -> XtreamPlaylistItem {
    let mut item = xtream_playlist_item(id);
    item.xtream_cluster = XtreamCluster::Series;
    item.item_type = PlaylistItemType::SeriesInfo;
    item.url = "".intern();
    let series_props =
        shared::model::SeriesStreamProperties { name: format!("Series {id}").intern(), details, ..Default::default() };
    item.additional_properties = Some(shared::model::StreamProperties::Series(Box::new(series_props)));
    item
}
