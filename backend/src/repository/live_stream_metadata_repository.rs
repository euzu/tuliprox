use crate::{
    model::{AppConfig, ConfigInput},
    repository::{
        build_input_storage_path, get_input_m3u_playlist_file_path, xtream_get_file_path, BPlusTreeQuery,
        BPlusTreeUpdate,
    },
    utils::file_exists_async,
};
use shared::{
    error::TuliproxError,
    model::{LiveStreamProperties, M3uPlaylistItem, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem},
};
use std::sync::Arc;

/// Semantic outcome of a live-bitrate persistence attempt.
///
/// Repository access failures remain `Err` so callers can retain the original error
/// category while handling non-error persistence outcomes exhaustively.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum LiveBitratePersistenceOutcome {
    Updated,
    AlreadyEqualOrHigher,
    PermanentlyInapplicable(LiveBitratePersistenceInapplicableReason),
    MissingDatabase,
    MissingStreamItem,
}

/// Permanent reason why live-bitrate metadata cannot be stored for an input item.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum LiveBitratePersistenceInapplicableReason {
    InvalidBitrate,
    InvalidStreamIdentity,
    UnsupportedInputType,
    IncompatibleStreamMetadata,
}

impl LiveBitratePersistenceInapplicableReason {
    pub(crate) const fn log_label(self) -> &'static str {
        match self {
            Self::InvalidBitrate => "invalid_bitrate",
            Self::InvalidStreamIdentity => "invalid_stream_identity",
            Self::UnsupportedInputType => "unsupported_input_type",
            Self::IncompatibleStreamMetadata => "incompatible_stream_metadata",
        }
    }
}

/// Loads measured live bitrate metadata without creating or mutating input storage.
pub(crate) async fn load_input_live_bitrate_bps(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    stream_ref: &str,
) -> Result<Option<u32>, TuliproxError> {
    let storage_path = {
        let config = app_config.config.load();
        build_input_storage_path(&input.name, &config.storage_dir)
    };

    if input.input_type.is_xtream() {
        let Ok(stream_id) = stream_ref.parse::<u32>() else {
            return Ok(None);
        };
        let path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
        if !file_exists_async(&path).await {
            return Ok(None);
        }
        let file_lock = app_config.file_locks.read_lock(&path).await;
        return tokio::task::spawn_blocking(move || {
            let _file_lock = file_lock;
            let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&path)
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to open live metadata: {err}")))?;
            query
                .query_zero_copy(&stream_id)
                .map(live_bitrate_from_xtream_item)
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to read live metadata: {err}")))
        })
        .await
        .map_err(|err| TuliproxError::RepositoryXtream(format!("live metadata read task failed: {err}")))?;
    }

    if input.input_type.is_m3u() {
        let path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
        if !file_exists_async(&path).await {
            return Ok(None);
        }
        let file_lock = app_config.file_locks.read_lock(&path).await;
        let stream_ref = Arc::<str>::from(stream_ref);
        return tokio::task::spawn_blocking(move || {
            let _file_lock = file_lock;
            let mut query = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(&path)
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to open live metadata: {err}")))?;
            query
                .query(&stream_ref)
                .map(live_bitrate_from_m3u_item)
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to read live metadata: {err}")))
        })
        .await
        .map_err(|err| TuliproxError::RepositoryM3u(format!("live metadata read task failed: {err}")))?;
    }

    Ok(None)
}

/// Persists a higher measured bitrate for an existing input live stream.
pub(crate) async fn persist_input_live_bitrate_bps(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    stream_ref: &str,
    bitrate_bps: u32,
) -> Result<LiveBitratePersistenceOutcome, TuliproxError> {
    if bitrate_bps == 0 {
        return Ok(LiveBitratePersistenceOutcome::PermanentlyInapplicable(
            LiveBitratePersistenceInapplicableReason::InvalidBitrate,
        ));
    }
    if !(input.input_type.is_xtream() || input.input_type.is_m3u()) {
        return Ok(LiveBitratePersistenceOutcome::PermanentlyInapplicable(
            LiveBitratePersistenceInapplicableReason::UnsupportedInputType,
        ));
    }
    let storage_path = {
        let config = app_config.config.load();
        build_input_storage_path(&input.name, &config.storage_dir)
    };

    if input.input_type.is_xtream() {
        let Ok(stream_id) = stream_ref.parse::<u32>() else {
            return Ok(LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::InvalidStreamIdentity,
            ));
        };
        let path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
        if !file_exists_async(&path).await {
            return Ok(LiveBitratePersistenceOutcome::MissingDatabase);
        }
        let file_lock = app_config.file_locks.write_lock(&path).await;
        return tokio::task::spawn_blocking(move || {
            let _file_lock = file_lock;
            let mut tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(&path)
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to open live metadata: {err}")))?;
            let Some(mut item) = tree
                .query(&stream_id)
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to read live metadata: {err}")))?
            else {
                return Ok(LiveBitratePersistenceOutcome::MissingStreamItem);
            };
            let outcome = raise_live_bitrate(&mut item.additional_properties, item.item_type, bitrate_bps);
            if outcome != LiveBitratePersistenceOutcome::Updated {
                return Ok(outcome);
            }
            tree.update(&stream_id, item)
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to update live metadata: {err}")))?;
            tree.commit()
                .map_err(|err| TuliproxError::RepositoryXtream(format!("failed to commit live metadata: {err}")))?;
            Ok(LiveBitratePersistenceOutcome::Updated)
        })
        .await
        .map_err(|err| TuliproxError::RepositoryXtream(format!("live metadata update task failed: {err}")))?;
    }

    if input.input_type.is_m3u() {
        let path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
        if !file_exists_async(&path).await {
            return Ok(LiveBitratePersistenceOutcome::MissingDatabase);
        }
        let file_lock = app_config.file_locks.write_lock(&path).await;
        let stream_ref = Arc::<str>::from(stream_ref);
        return tokio::task::spawn_blocking(move || {
            let _file_lock = file_lock;
            let mut tree = BPlusTreeUpdate::<Arc<str>, M3uPlaylistItem>::try_new_with_backoff(&path)
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to open live metadata: {err}")))?;
            let Some(mut item) = tree
                .query(&stream_ref)
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to read live metadata: {err}")))?
            else {
                return Ok(LiveBitratePersistenceOutcome::MissingStreamItem);
            };
            let outcome = raise_live_bitrate(&mut item.additional_properties, item.item_type, bitrate_bps);
            if outcome != LiveBitratePersistenceOutcome::Updated {
                return Ok(outcome);
            }
            tree.update(&stream_ref, item)
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to update live metadata: {err}")))?;
            tree.commit()
                .map_err(|err| TuliproxError::RepositoryM3u(format!("failed to commit live metadata: {err}")))?;
            Ok(LiveBitratePersistenceOutcome::Updated)
        })
        .await
        .map_err(|err| TuliproxError::RepositoryM3u(format!("live metadata update task failed: {err}")))?;
    }

    Ok(LiveBitratePersistenceOutcome::PermanentlyInapplicable(
        LiveBitratePersistenceInapplicableReason::UnsupportedInputType,
    ))
}

fn raise_live_bitrate(
    additional_properties: &mut Option<StreamProperties>,
    item_type: PlaylistItemType,
    bitrate_bps: u32,
) -> LiveBitratePersistenceOutcome {
    match additional_properties {
        Some(StreamProperties::Live(properties)) if bitrate_bps > properties.bitrate => {
            properties.bitrate = bitrate_bps;
            LiveBitratePersistenceOutcome::Updated
        }
        Some(StreamProperties::Live(_)) => LiveBitratePersistenceOutcome::AlreadyEqualOrHigher,
        None if item_type.is_live() => {
            *additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                bitrate: bitrate_bps,
                ..LiveStreamProperties::default()
            })));
            LiveBitratePersistenceOutcome::Updated
        }
        Some(StreamProperties::Video(_) | StreamProperties::Series(_) | StreamProperties::Episode(_)) | None => {
            LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::IncompatibleStreamMetadata,
            )
        }
    }
}

fn live_bitrate(properties: Option<StreamProperties>) -> Option<u32> {
    match properties {
        Some(StreamProperties::Live(properties)) if properties.bitrate > 0 => Some(properties.bitrate),
        Some(
            StreamProperties::Live(_)
            | StreamProperties::Video(_)
            | StreamProperties::Series(_)
            | StreamProperties::Episode(_),
        )
        | None => None,
    }
}

fn live_bitrate_from_xtream_item(item: Option<XtreamPlaylistItem>) -> Option<u32> {
    item.and_then(|item| live_bitrate(item.additional_properties))
}

fn live_bitrate_from_m3u_item(item: Option<M3uPlaylistItem>) -> Option<u32> {
    item.and_then(|item| live_bitrate(item.additional_properties))
}

#[cfg(test)]
mod tests {
    use super::{
        load_input_live_bitrate_bps, persist_input_live_bitrate_bps, LiveBitratePersistenceInapplicableReason,
        LiveBitratePersistenceOutcome,
    };
    use crate::{
        model::{
            ApiProxyConfig, AppConfig, Config, ConfigInput, CustomStreamResponse, HdHomeRunConfig,
            MediaToolCapabilities, SourcesConfig,
        },
        repository::{
            build_input_storage_path, get_input_m3u_playlist_file_path, xtream_get_file_path, BPlusTree,
            BPlusTreeQuery,
        },
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{
        ConfigPaths, InputType, LiveStreamProperties, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        StreamProperties, XtreamCluster,
    };
    use std::sync::Arc;

    fn test_app_config(storage_dir: &str) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                storage_dir: storage_dir.to_string(),
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

    fn playlist_item(input: &ConfigInput, stream_ref: &str, bitrate: u32) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: Arc::from(stream_ref),
                input_stream_id: Arc::from(stream_ref),
                input_name: Arc::clone(&input.name),
                item_type: PlaylistItemType::LiveHls,
                xtream_cluster: XtreamCluster::Live,
                additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                    bitrate,
                    ..LiveStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    #[tokio::test]
    async fn loads_positive_xtream_and_m3u_live_bitrate_and_normalizes_zero() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_config = test_app_config(temp.path().to_string_lossy().as_ref());

        let xtream_input = ConfigInput {
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let xtream_storage = build_input_storage_path(&xtream_input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&xtream_storage).expect("xtream storage");
        let xtream_item = shared::model::XtreamPlaylistItem::from(&playlist_item(&xtream_input, "42", 2_500_000));
        let mut xtream_tree = BPlusTree::new();
        xtream_tree.insert(42_u32, xtream_item);
        xtream_tree
            .store(&xtream_get_file_path(&xtream_storage, XtreamCluster::Live))
            .expect("xtream tree");

        let m3u_input = ConfigInput {
            name: Arc::from("m3u-input"),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let m3u_storage = build_input_storage_path(&m3u_input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&m3u_storage).expect("m3u storage");
        let m3u_item =
            shared::model::M3uPlaylistItem::from(&playlist_item(&m3u_input, "channel-a", 2_000_000));
        let m3u_zero_item =
            shared::model::M3uPlaylistItem::from(&playlist_item(&m3u_input, "channel-zero", 0));
        let mut m3u_tree = BPlusTree::new();
        m3u_tree.insert(Arc::<str>::from("channel-a"), m3u_item);
        m3u_tree.insert(Arc::<str>::from("channel-zero"), m3u_zero_item);
        m3u_tree
            .store(&get_input_m3u_playlist_file_path(&m3u_storage, &m3u_input.name))
            .expect("m3u tree");

        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &xtream_input, "42").await.expect("xtream read"),
            Some(2_500_000)
        );
        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &m3u_input, "channel-a").await.expect("m3u read"),
            Some(2_000_000)
        );
        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &m3u_input, "channel-zero")
                .await
                .expect("m3u zero read"),
            None
        );
    }

    #[tokio::test]
    async fn missing_invalid_or_unsupported_input_metadata_is_unknown() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_config = test_app_config(temp.path().to_string_lossy().as_ref());
        let xtream_input = ConfigInput {
            name: Arc::from("missing-xtream"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let unsupported_input = ConfigInput {
            name: Arc::from("library-input"),
            input_type: InputType::Library,
            ..ConfigInput::default()
        };

        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &xtream_input, "not-numeric")
                .await
                .expect("invalid stream ref"),
            None
        );
        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &xtream_input, "42").await.expect("missing db"),
            None
        );
        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &unsupported_input, "42")
                .await
                .expect("unsupported input"),
            None
        );
    }

    #[tokio::test]
    async fn persists_only_higher_positive_live_bitrate_and_preserves_metadata() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_config = test_app_config(temp.path().to_string_lossy().as_ref());
        let xtream_input = ConfigInput {
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let xtream_storage = build_input_storage_path(&xtream_input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&xtream_storage).expect("xtream storage");
        let mut source_item = playlist_item(&xtream_input, "42", 1_000_000);
        let Some(StreamProperties::Live(properties)) = source_item.header.additional_properties.as_mut() else {
            panic!("live properties");
        };
        properties.video = Some(Arc::from("video-metadata"));
        properties.audio = Some(Arc::from("audio-metadata"));
        properties.last_probed_timestamp = Some(123);
        properties.last_success_timestamp = Some(120);
        let mut tree = BPlusTree::new();
        tree.insert(42_u32, shared::model::XtreamPlaylistItem::from(&source_item));
        let xtream_path = xtream_get_file_path(&xtream_storage, XtreamCluster::Live);
        tree.store(&xtream_path).expect("xtream tree");

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &xtream_input, "42", 2_500_000)
                .await
                .expect("higher bitrate update"),
            LiveBitratePersistenceOutcome::Updated
        );
        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &xtream_input, "42", 2_000_000)
                .await
                .expect("lower bitrate ignored"),
            LiveBitratePersistenceOutcome::AlreadyEqualOrHigher
        );
        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &xtream_input, "42", 0)
                .await
                .expect("zero bitrate ignored"),
            LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::InvalidBitrate
            )
        );

        let mut query = BPlusTreeQuery::<u32, shared::model::XtreamPlaylistItem>::try_new(&xtream_path)
            .expect("updated xtream tree");
        let stored = query.query(&42).expect("stored xtream item").expect("xtream item");
        let Some(StreamProperties::Live(properties)) = stored.additional_properties else {
            panic!("stored live properties");
        };
        assert_eq!(properties.bitrate, 2_500_000);
        assert_eq!(properties.video.as_deref(), Some("video-metadata"));
        assert_eq!(properties.audio.as_deref(), Some("audio-metadata"));
        assert_eq!(properties.last_probed_timestamp, Some(123));
        assert_eq!(properties.last_success_timestamp, Some(120));

        let m3u_input = ConfigInput {
            name: Arc::from("m3u-input"),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let m3u_storage = build_input_storage_path(&m3u_input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&m3u_storage).expect("m3u storage");
        let mut m3u_item = shared::model::M3uPlaylistItem::from(&playlist_item(&m3u_input, "channel-a", 0));
        m3u_item.additional_properties = None;
        let mut tree = BPlusTree::new();
        tree.insert(Arc::<str>::from("channel-a"), m3u_item);
        let m3u_path = get_input_m3u_playlist_file_path(&m3u_storage, &m3u_input.name);
        tree.store(&m3u_path).expect("m3u tree");

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &m3u_input, "channel-a", 1_750_000)
                .await
                .expect("m3u bitrate update"),
            LiveBitratePersistenceOutcome::Updated
        );
        assert_eq!(
            load_input_live_bitrate_bps(&app_config, &m3u_input, "channel-a")
                .await
                .expect("m3u bitrate read"),
            Some(1_750_000)
        );
    }

    #[tokio::test]
    async fn persistence_distinguishes_missing_database_and_stream_item() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_config = test_app_config(temp.path().to_string_lossy().as_ref());
        let input = ConfigInput {
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &input, "42", 2_000_000)
                .await
                .expect("missing database outcome"),
            LiveBitratePersistenceOutcome::MissingDatabase
        );

        let storage = build_input_storage_path(&input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&storage).expect("xtream storage");
        let mut tree = BPlusTree::new();
        tree.insert(
            7_u32,
            shared::model::XtreamPlaylistItem::from(&playlist_item(&input, "7", 1_000_000)),
        );
        tree.store(&xtream_get_file_path(&storage, XtreamCluster::Live))
            .expect("xtream tree");

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &input, "42", 2_000_000)
                .await
                .expect("missing item outcome"),
            LiveBitratePersistenceOutcome::MissingStreamItem
        );
    }

    #[tokio::test]
    async fn persistence_distinguishes_permanently_inapplicable_inputs_and_metadata() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_config = test_app_config(temp.path().to_string_lossy().as_ref());
        let xtream_input = ConfigInput {
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let unsupported_input = ConfigInput {
            name: Arc::from("library-input"),
            input_type: InputType::Library,
            ..ConfigInput::default()
        };

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &xtream_input, "not-numeric", 2_000_000)
                .await
                .expect("invalid identity outcome"),
            LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::InvalidStreamIdentity
            )
        );
        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &unsupported_input, "42", 2_000_000)
                .await
                .expect("unsupported input outcome"),
            LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::UnsupportedInputType
            )
        );

        let m3u_input = ConfigInput {
            name: Arc::from("m3u-input"),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let storage = build_input_storage_path(&m3u_input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&storage).expect("m3u storage");
        let mut item = shared::model::M3uPlaylistItem::from(&playlist_item(&m3u_input, "channel-a", 0));
        item.additional_properties = Some(StreamProperties::Video(Box::default()));
        let mut tree = BPlusTree::new();
        tree.insert(Arc::<str>::from("channel-a"), item);
        tree.store(&get_input_m3u_playlist_file_path(&storage, &m3u_input.name))
            .expect("m3u tree");

        assert_eq!(
            persist_input_live_bitrate_bps(&app_config, &m3u_input, "channel-a", 2_000_000)
                .await
                .expect("incompatible metadata outcome"),
            LiveBitratePersistenceOutcome::PermanentlyInapplicable(
                LiveBitratePersistenceInapplicableReason::IncompatibleStreamMetadata
            )
        );
    }
}
