use async_compression::tokio::bufread::GzipDecoder;
use futures::StreamExt;
use log::{error, info, warn};
use parking_lot::Mutex;
use shared::{
    defaults::default_imdb_url,
    model::{ImdbRatingSource, InputFetchMethod, PlaylistItem, PlaylistItemHeader, StreamProperties},
    utils::{is_valid_provider_rating, parse_imdb_id_from_title, Internable},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;
use tuliprox_core::{
    model::{AppConfig, ImdbConfig, InputSource},
    utils::request::{send_input_with_retry_and_provider_policy_with_options_result, RequestFetchOptions},
};
use tuliprox_repository::{
    get_imdb_ratings_path, imdb_ratings_row_count, store_imdb_tree, ImdbIndexBuilder, ImdbLookup, ImdbRating, ImdbTree,
    ParseStats,
};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum ImdbUpdateError {
    #[error("IMDb ratings update is disabled")]
    Disabled,
    #[error("IMDb ratings update already in progress")]
    AlreadyRunning,
    #[error("Failed to download IMDb ratings: {0}")]
    DownloadFailed(String),
    #[error("Failed to process IMDb ratings: {0}")]
    ProcessFailed(String),
    #[error("IMDb ratings update rejected: {0}")]
    Rejected(String),
}

pub async fn update_imdb_ratings(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
) -> Result<usize, ImdbUpdateError> {
    let config = app_config.config.load();
    let Some(imdb_cfg) = config.metadata_update.as_ref().map(|m| &m.imdb) else {
        return Err(ImdbUpdateError::Disabled);
    };
    if !imdb_cfg.enabled {
        return Err(ImdbUpdateError::Disabled);
    }

    let ratings_path = get_imdb_ratings_path(&config.storage_dir);
    if let Some(parent) = ratings_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ImdbUpdateError::ProcessFailed(format!("Failed to create storage dir: {e}")))?;
    }

    let Ok(_file_lock) = app_config.file_locks.try_write_lock(&ratings_path).await else {
        info!("IMDb ratings update already in progress; skipping concurrent run");
        return Err(ImdbUpdateError::AlreadyRunning);
    };

    let url_str = if imdb_cfg.url.trim().is_empty() { default_imdb_url() } else { imdb_cfg.url.clone() };

    let parsed_url = Url::parse(&url_str).map_err(|e| ImdbUpdateError::DownloadFailed(e.to_string()))?;

    let (mut tree, stats) = if parsed_url.scheme() == "file" {
        let file_path =
            parsed_url.to_file_path().map_err(|()| ImdbUpdateError::DownloadFailed("Invalid file URL".to_string()))?;
        let file =
            tokio::fs::File::open(&file_path).await.map_err(|e| ImdbUpdateError::DownloadFailed(e.to_string()))?;
        let buf_reader = BufReader::new(file);
        stream_and_parse_gzip(buf_reader).await?
    } else {
        let input_source = InputSource {
            name: "IMDb".intern(),
            url: url_str,
            provider: None,
            username: None,
            password: None,
            method: InputFetchMethod::GET,
            headers: HashMap::default(),
        };

        let response = send_input_with_retry_and_provider_policy_with_options_result(
            app_config,
            http_client,
            &input_source,
            None,
            &parsed_url,
            RequestFetchOptions::default(),
        )
        .await
        .map_err(|err| ImdbUpdateError::DownloadFailed(err.to_string()))?
        .response;

        if !response.status().is_success() {
            return Err(ImdbUpdateError::DownloadFailed(format!("HTTP error: {}", response.status())));
        }

        let stream = response.bytes_stream().map(|res| {
            res.map_err(|err| {
                let kind = if err.is_timeout() { io::ErrorKind::TimedOut } else { io::ErrorKind::Other };
                io::Error::new(kind, err)
            })
        });
        let stream_reader = StreamReader::new(stream);
        let buf_reader = BufReader::new(stream_reader);
        stream_and_parse_gzip(buf_reader).await?
    };

    let new_len = tree.len();
    if new_len == 0 {
        return Err(ImdbUpdateError::Rejected("Dataset has 0 rows".to_string()));
    }

    // The current table on disk is the reference for the shrink guard. A
    // missing or unreadable file has no reference to compare against.
    if let Ok(current_len) = imdb_ratings_row_count(&ratings_path) {
        if new_len < current_len / 2 {
            return Err(ImdbUpdateError::Rejected(format!(
                "Dataset row count ({new_len}) is fewer than half of current index ({current_len})"
            )));
        }
    }

    let store_path = ratings_path.clone();
    tokio::task::spawn_blocking(move || store_imdb_tree(&mut tree, &store_path))
        .await
        .map_err(|err| ImdbUpdateError::ProcessFailed(format!("IMDb ratings store task failed: {err}")))?
        .map_err(|err| ImdbUpdateError::ProcessFailed(err.to_string()))?;

    info!(
        "IMDb ratings updated: {} rows ({} skipped, {} duplicates)",
        new_len, stats.skipped_rows, stats.duplicate_rows
    );

    Ok(new_len)
}

/// Runs [`update_imdb_ratings`] in the background and logs a failure. A disabled
/// feature is not a failure. Used at startup and on config reload, where nobody
/// waits for the download.
pub fn spawn_imdb_ratings_update(app_config: Arc<AppConfig>, http_client: reqwest::Client) {
    tokio::spawn(async move {
        match update_imdb_ratings(&app_config, &http_client).await {
            Ok(_) | Err(ImdbUpdateError::Disabled | ImdbUpdateError::AlreadyRunning) => {}
            Err(err) => error!("Background IMDb ratings download failed: {err}"),
        }
    });
}

/// The gzip decoder validates the header itself, across chunk boundaries; a
/// body that is not gzip fails on the first read.
async fn stream_and_parse_gzip<R: tokio::io::AsyncRead + Unpin>(
    reader: BufReader<R>,
) -> Result<(ImdbTree, ParseStats), ImdbUpdateError> {
    let gzip_decoder = GzipDecoder::new(reader);
    let mut text_reader = BufReader::new(gzip_decoder);

    let mut builder = ImdbIndexBuilder::new();
    let mut line = String::new();

    while text_reader.read_line(&mut line).await.map_err(|e| ImdbUpdateError::ProcessFailed(e.to_string()))? > 0 {
        builder.push_line(&line);
        line.clear();
    }

    Ok(builder.finish())
}

/// One run's view of the `IMDb` ratings: the config valid at the start of the run
/// and the lookup opened for it on first use. Both are taken once per run, so
/// every target of a run sees the same ratings even when the dataset is replaced
/// mid-run. The lookup is dropped with the run, so no table memory survives it.
#[derive(Clone, Default)]
pub struct ImdbRatingRuntime {
    inner: Arc<ImdbRatingRuntimeInner>,
}

#[derive(Default)]
struct ImdbRatingRuntimeInner {
    config: ImdbConfig,
    /// `None` for a runtime that only ever applies an already opened lookup
    /// (tests and callers that bring their own table).
    app_config: Option<Arc<AppConfig>>,
    lookup: Mutex<Option<ImdbLookup>>,
    /// Set by the first open attempt, so a run opens the table at most once and
    /// reports a missing table at most once.
    open_attempted: AtomicBool,
    /// Serializes concurrent open attempts of one run; a `tokio` mutex because
    /// the open awaits the file lock.
    open_gate: tokio::sync::Mutex<()>,
}

impl ImdbRatingRuntime {
    #[must_use]
    pub fn new(config: ImdbConfig, lookup: Option<ImdbLookup>) -> Self {
        let attempted = lookup.is_some();
        Self {
            inner: Arc::new(ImdbRatingRuntimeInner {
                config,
                app_config: None,
                lookup: Mutex::new(lookup),
                open_attempted: AtomicBool::new(attempted),
                open_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// A runtime whose table is opened lazily by the first [`Self::ensure_open`]
    /// call that meets a rateable item. `disk_based_processing` decides whether
    /// the table is read into memory once or queried on disk.
    #[must_use]
    pub fn for_run(app_config: Arc<AppConfig>, config: ImdbConfig) -> Self {
        Self {
            inner: Arc::new(ImdbRatingRuntimeInner {
                config,
                app_config: Some(app_config),
                ..ImdbRatingRuntimeInner::default()
            }),
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool { self.inner.config.enabled }

    /// True once the run has a ratings table open.
    #[must_use]
    pub fn is_opened(&self) -> bool { self.inner.lookup.lock().is_some() }

    /// True once the run tried to open the table, whether it found one or not.
    /// A run that never meets a rateable item leaves this `false`.
    #[must_use]
    pub fn is_open_attempted(&self) -> bool { self.inner.open_attempted.load(Ordering::Acquire) }

    /// Opens the run's table if `items` contains something the ratings apply to.
    ///
    /// Returns immediately when the feature is disabled, when the table is
    /// already open (or the open was already attempted), or when no item has
    /// video or series stream properties - a live-only update never touches the
    /// table. The open is idempotent, so it is safe to call it before every
    /// batch.
    ///
    /// The file read lock is held for the read only; the lookup then relies on
    /// the table being replaced atomically, not on the lock.
    pub async fn ensure_open(&self, items: &[PlaylistItem]) {
        let inner = &self.inner;
        if !inner.config.enabled || inner.open_attempted.load(Ordering::Acquire) || inner.app_config.is_none() {
            return;
        }
        if inner.lookup.lock().is_some() || !has_rateable_items(items) {
            return;
        }

        let _gate = inner.open_gate.lock().await;
        // Another target may have opened the table while this task waited. The
        // flag is published only after the outcome is stored (below), so a caller
        // that sees it never applies ratings before the lookup is in place.
        if inner.open_attempted.load(Ordering::Acquire) {
            return;
        }
        let Some(app_config) = inner.app_config.clone() else { return };
        let (storage_dir, disk_based) = {
            let cfg = app_config.config.load();
            (cfg.storage_dir.clone(), cfg.disk_based_processing)
        };
        let path = get_imdb_ratings_path(&storage_dir);
        // The read lock covers the read; it is released before the run uses the
        // lookup. The read itself is blocking, so it must not run on the async
        // worker.
        let opened = {
            let _file_lock = app_config.file_locks.read_lock(&path).await;
            let read_path = path.clone();
            tokio::task::spawn_blocking(move || ImdbLookup::open(&read_path, disk_based)).await
        };
        match opened {
            Ok(Ok(Some(lookup))) => *inner.lookup.lock() = Some(lookup),
            Ok(Ok(None)) => {
                warn!("IMDb ratings are enabled but no ratings table is stored; the feature is a no-op for this run");
            }
            Ok(Err(err)) => {
                warn!("Failed to open the IMDb ratings table ({err}); the feature is a no-op for this run");
            }
            Err(err) => {
                warn!("Failed to open the IMDb ratings table (read task failed: {err}); the feature is a no-op for this run");
            }
        }
        inner.open_attempted.store(true, Ordering::Release);
    }

    pub fn apply(&self, items: &mut [PlaylistItem]) {
        // The lock is never held across an `.await`; `apply` is synchronous.
        let mut lookup = self.inner.lookup.lock();
        apply_imdb_ratings(items, lookup.as_mut(), &self.inner.config);
    }
}

/// True when at least one item carries the stream properties ratings apply to.
fn has_rateable_items(items: &[PlaylistItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item.header.additional_properties.as_ref(),
            Some(StreamProperties::Video(_) | StreamProperties::Series(_))
        )
    })
}

/// Fills the `IMDb` id, rating, votes and the effective rating of every video and
/// series item.
///
/// Runs before a target's filter stage, so the `Rating` and `Votes` fields see the
/// values of the current update. It also runs for targets without a filter, which
/// keeps the stored fields consistent.
pub fn apply_imdb_ratings(items: &mut [PlaylistItem], mut lookup: Option<&mut ImdbLookup>, cfg: &ImdbConfig) {
    for item in items {
        let PlaylistItemHeader { additional_properties, title, name, .. } = &mut item.header;
        let Some(properties) = additional_properties.as_mut() else { continue };
        match properties {
            StreamProperties::Video(video) => apply_rating_fields(
                &mut RatingFields {
                    imdb: &mut video.imdb,
                    imdb_rating: &mut video.imdb_rating,
                    imdb_votes: &mut video.imdb_votes,
                    effective_rating: &mut video.effective_rating,
                },
                valid_provider_rating(video.rating),
                title,
                name,
                lookup.as_deref_mut(),
                cfg,
            ),
            StreamProperties::Series(series) => apply_rating_fields(
                &mut RatingFields {
                    imdb: &mut series.imdb,
                    imdb_rating: &mut series.imdb_rating,
                    imdb_votes: &mut series.imdb_votes,
                    effective_rating: &mut series.effective_rating,
                },
                valid_provider_rating(Some(series.rating)),
                title,
                name,
                lookup.as_deref_mut(),
                cfg,
            ),
            StreamProperties::Live(_) | StreamProperties::Episode(_) => {}
        }
    }
}

/// The provider rating as the stored `f32`, or `None` when it is outside the 0-10 scale.
///
/// The range is checked on the original `f64`: narrowing first would round a value just above 10 down to 10.
fn valid_provider_rating(rating: Option<f64>) -> Option<f32> {
    rating.filter(|&value| is_valid_provider_rating(value)).map(rating_to_f32)
}

/// Narrows a provider rating (0-10) to the stored `f32`. The value range and the
/// one or two decimals providers send fit `f32` without a visible loss.
#[allow(clippy::cast_possible_truncation)]
fn rating_to_f32(rating: f64) -> f32 { rating as f32 }

struct RatingFields<'a> {
    imdb: &'a mut Option<u32>,
    imdb_rating: &'a mut Option<f32>,
    imdb_votes: &'a mut Option<u32>,
    effective_rating: &'a mut Option<f32>,
}

fn apply_rating_fields(
    fields: &mut RatingFields<'_>,
    provider_rating: Option<f32>,
    title: &str,
    name: &str,
    lookup: Option<&mut ImdbLookup>,
    cfg: &ImdbConfig,
) {
    // An existing id is never overwritten: a persisted or provider-sourced id is
    // more authoritative than a title tag.
    if fields.imdb.is_none() {
        if let Some(id) = parse_imdb_id_from_title(title).or_else(|| parse_imdb_id_from_title(name)) {
            *fields.imdb = Some(id);
        }
    }

    let found = match (cfg.enabled, lookup, *fields.imdb) {
        (true, Some(lookup), Some(id)) => lookup.lookup(id),
        _ => None,
    };
    *fields.imdb_rating = found.map(|rating| rating.rating);
    *fields.imdb_votes = found.map(|rating| rating.votes);

    *fields.effective_rating = if cfg.enabled {
        match cfg.rating_source {
            ImdbRatingSource::ImdbThenProvider => imdb_effective_rating(found, cfg.min_votes).or(provider_rating),
            ImdbRatingSource::Imdb => imdb_effective_rating(found, cfg.min_votes),
            ImdbRatingSource::Provider => provider_rating,
        }
    } else {
        provider_rating
    };
}

fn imdb_effective_rating(lookup: Option<ImdbRating>, min_votes: u32) -> Option<f32> {
    lookup.filter(|found| found.votes >= min_votes).map(|found| found.rating)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwapOption;
    use flate2::{write::GzEncoder, Compression};
    use shared::model::{LiveStreamProperties, SeriesStreamProperties, VideoStreamProperties, XtreamCluster};
    use std::{io::Write, path::Path};
    use tokio::net::TcpListener;
    use tuliprox_core::model::{Config, ImdbConfig, MetadataUpdateConfig};
    use tuliprox_repository::{ImdbEntry, ImdbLookup, ImdbRow, ImdbTree};

    fn make_test_app_config(storage_dir: &str, url: &str, enabled: bool) -> Arc<AppConfig> {
        let mut config = Config { storage_dir: storage_dir.to_string(), ..Default::default() };
        config.metadata_update = Some(MetadataUpdateConfig {
            imdb: ImdbConfig { enabled, url: url.to_string(), ..Default::default() },
            ..Default::default()
        });
        Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            plugins: Arc::default(),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: storage_dir.to_string(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                failover_file_path: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
            failover: Arc::default(),
        })
    }

    fn compress_to_gzip(content: &str) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content.as_bytes()).expect("compress gzip");
        encoder.finish().expect("finish gzip")
    }

    async fn start_test_server(body: Vec<u8>, status_code: u16) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;

                let response_headers = format!(
                    "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    status_code,
                    body.len()
                );
                let _ = socket.write_all(response_headers.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// `rows` are `(id, rating in tenths, votes)`.
    fn test_rows(rows: &[(u32, u16, u32)]) -> Vec<ImdbRow> {
        rows.iter().map(|(id, rating_x10, votes)| ImdbRow { id: *id, votes: *votes, rating_x10: *rating_x10 }).collect()
    }

    fn test_tree(rows: &[(u32, u16, u32)]) -> ImdbTree {
        let mut tree = ImdbTree::new();
        for row in test_rows(rows) {
            tree.insert(row.id, ImdbEntry::from(&row));
        }
        tree
    }

    fn store_test_tree(path: &Path, rows: &[(u32, u16, u32)]) {
        let mut tree = test_tree(rows);
        store_imdb_tree(&mut tree, path).expect("store ratings table");
    }

    fn test_lookup(rows: &[(u32, u16, u32)]) -> ImdbLookup { ImdbLookup::from_rows(test_rows(rows)) }

    fn enabled_config(min_votes: u32, rating_source: ImdbRatingSource) -> ImdbConfig {
        ImdbConfig { enabled: true, min_votes, rating_source, ..Default::default() }
    }

    fn video_item(title: &str, provider_rating: Option<f64>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                name: "".intern(),
                xtream_cluster: XtreamCluster::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    rating: provider_rating,
                    ..VideoStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn series_item(title: &str, provider_rating: f64) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                name: "".intern(),
                xtream_cluster: XtreamCluster::Series,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    rating: provider_rating,
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn live_item(title: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                name: "".intern(),
                xtream_cluster: XtreamCluster::Live,
                additional_properties: Some(StreamProperties::Live(Box::default())),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn video_props(item: &PlaylistItem) -> &VideoStreamProperties {
        match item.header.additional_properties.as_ref() {
            Some(StreamProperties::Video(video)) => video,
            other => panic!("expected video properties, got {other:?}"),
        }
    }

    fn series_props(item: &PlaylistItem) -> &SeriesStreamProperties {
        match item.header.additional_properties.as_ref() {
            Some(StreamProperties::Series(series)) => series,
            other => panic!("expected series properties, got {other:?}"),
        }
    }

    #[test]
    fn imdb_then_provider_prefers_imdb_above_min_votes() {
        let mut lookup = test_lookup(&[(6_549_510, 42, 5_000)]);
        let mut items = [video_item("Kill Em All 2 imdb=tt6549510", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(1_000, ImdbRatingSource::ImdbThenProvider));

        let props = video_props(&items[0]);
        assert_eq!(props.imdb, Some(6_549_510));
        assert_eq!(props.imdb_rating, Some(4.2));
        assert_eq!(props.imdb_votes, Some(5_000));
        assert_eq!(props.effective_rating, Some(4.2));
        assert_eq!(props.rating, Some(6.8));
    }

    #[test]
    fn imdb_then_provider_falls_back_to_provider_below_min_votes() {
        let mut lookup = test_lookup(&[(6_549_510, 89, 40)]);
        let mut items = [video_item("Movie [tt6549510]", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(1_000, ImdbRatingSource::ImdbThenProvider));

        let props = video_props(&items[0]);
        // Raw IMDb values are stored regardless of the threshold.
        assert_eq!(props.imdb_rating, Some(8.9));
        assert_eq!(props.imdb_votes, Some(40));
        assert_eq!(props.effective_rating, Some(6.8));
    }

    #[test]
    fn imdb_then_provider_without_provider_rating_is_unrated() {
        let mut lookup = test_lookup(&[(6_549_510, 89, 40)]);
        let mut items = [video_item("Movie tt6549510", None)];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(1_000, ImdbRatingSource::ImdbThenProvider));

        assert_eq!(video_props(&items[0]).effective_rating, None);
    }

    #[test]
    fn strict_imdb_source_ignores_provider_rating() {
        let mut lookup = test_lookup(&[(1_111_111, 42, 5_000), (2_222_222, 89, 40)]);
        let mut items = [video_item("A [tt1111111]", Some(6.8)), video_item("B [tt2222222]", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(1_000, ImdbRatingSource::Imdb));

        assert_eq!(video_props(&items[0]).effective_rating, Some(4.2));
        assert_eq!(video_props(&items[1]).effective_rating, None);
        assert_eq!(video_props(&items[1]).imdb_rating, Some(8.9));
    }

    #[test]
    fn provider_source_keeps_provider_rating() {
        let mut lookup = test_lookup(&[(1_111_111, 42, 5_000)]);
        let mut items = [video_item("A [tt1111111]", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::Provider));

        let props = video_props(&items[0]);
        assert_eq!(props.effective_rating, Some(6.8));
        assert_eq!(props.imdb_rating, Some(4.2));
        assert_eq!(props.imdb_votes, Some(5_000));
    }

    #[test]
    fn disabled_keeps_provider_rating_and_drops_imdb_values() {
        let mut lookup = test_lookup(&[(1_111_111, 42, 5_000)]);
        let mut items = [video_item("A [tt1111111]", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &ImdbConfig::default());

        let props = video_props(&items[0]);
        assert_eq!(props.effective_rating, Some(6.8));
        assert_eq!(props.imdb_rating, None);
        assert_eq!(props.imdb_votes, None);
        // The title tag is still stored; it is an id source, not a rating source.
        assert_eq!(props.imdb, Some(1_111_111));
    }

    #[test]
    fn enabled_without_index_is_a_no_op_except_the_provider_fallback() {
        let mut items = [video_item("A [tt1111111]", Some(6.8)), video_item("B [tt1111111]", None)];

        apply_imdb_ratings(&mut items, None, &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        assert_eq!(video_props(&items[0]).effective_rating, Some(6.8));
        assert_eq!(video_props(&items[1]).effective_rating, None);
        assert_eq!(video_props(&items[0]).imdb_rating, None);
        assert_eq!(video_props(&items[0]).imdb_votes, None);

        let mut strict = [video_item("A [tt1111111]", Some(6.8))];
        apply_imdb_ratings(&mut strict, None, &enabled_config(0, ImdbRatingSource::Imdb));
        assert_eq!(video_props(&strict[0]).effective_rating, None);
    }

    #[test]
    fn provider_rating_outside_zero_to_ten_is_ignored() {
        let enabled = enabled_config(0, ImdbRatingSource::Provider);
        let disabled = ImdbConfig { enabled: false, ..enabled_config(0, ImdbRatingSource::Provider) };
        for cfg in [&enabled, &disabled] {
            let mut items = [
                video_item("A", Some(10.0)),
                video_item("B", Some(10.5)),
                video_item("C", Some(-1.0)),
                video_item("D", Some(0.1)),
                video_item("E", Some(10.000_000_1)),
            ];
            apply_imdb_ratings(&mut items, None, cfg);
            assert_eq!(video_props(&items[0]).effective_rating, Some(10.0));
            assert_eq!(video_props(&items[1]).effective_rating, None);
            assert_eq!(video_props(&items[2]).effective_rating, None);
            assert_eq!(video_props(&items[3]).effective_rating, Some(0.1));
            assert_eq!(video_props(&items[4]).effective_rating, None);
        }
    }

    #[test]
    fn min_votes_boundary_is_inclusive() {
        let mut lookup = test_lookup(&[(1_111_111, 75, 1_000)]);

        let mut at_boundary = [video_item("A [tt1111111]", Some(6.8))];
        apply_imdb_ratings(
            &mut at_boundary,
            Some(&mut lookup),
            &enabled_config(1_000, ImdbRatingSource::ImdbThenProvider),
        );
        assert_eq!(video_props(&at_boundary[0]).effective_rating, Some(7.5));

        let mut below_boundary = [video_item("A [tt1111111]", Some(6.8))];
        apply_imdb_ratings(
            &mut below_boundary,
            Some(&mut lookup),
            &enabled_config(1_001, ImdbRatingSource::ImdbThenProvider),
        );
        assert_eq!(video_props(&below_boundary[0]).effective_rating, Some(6.8));
    }

    #[test]
    fn min_votes_zero_keeps_every_imdb_rating() {
        let mut lookup = test_lookup(&[(1_111_111, 99, 1)]);
        let mut items = [video_item("A [tt1111111]", Some(6.8))];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        assert_eq!(video_props(&items[0]).effective_rating, Some(9.9));
    }

    #[test]
    fn existing_imdb_id_is_not_overwritten() {
        let mut lookup = test_lookup(&[(1_111_111, 75, 1_000), (2_222_222, 50, 1_000)]);
        let mut items = [video_item("A [tt1111111]", None)];
        if let Some(StreamProperties::Video(video)) = items[0].header.additional_properties.as_mut() {
            video.imdb = Some(2_222_222);
        }

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        let props = video_props(&items[0]);
        assert_eq!(props.imdb, Some(2_222_222));
        assert_eq!(props.effective_rating, Some(5.0));
    }

    #[test]
    fn title_tag_is_taken_from_the_name_when_the_title_has_none() {
        let mut items = [video_item("Kill Em All 2", None)];
        items[0].header.name = "Kill Em All 2 imdbid-tt6549510".intern();

        apply_imdb_ratings(&mut items, None, &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        assert_eq!(video_props(&items[0]).imdb, Some(6_549_510));
    }

    #[test]
    fn unknown_imdb_id_is_unrated() {
        let mut lookup = test_lookup(&[(1_111_111, 75, 1_000)]);
        let mut items = [video_item("A [tt9999999]", None)];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        let props = video_props(&items[0]);
        assert_eq!(props.imdb, Some(9_999_999));
        assert_eq!(props.imdb_rating, None);
        assert_eq!(props.effective_rating, None);
    }

    #[test]
    fn series_ratings_are_applied() {
        let mut lookup = test_lookup(&[(6_549_510, 42, 5_000)]);
        let mut items = [series_item("Series [tt6549510]", 3.0), series_item("Unrated series", 0.0)];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        let rated = series_props(&items[0]);
        assert_eq!(rated.imdb, Some(6_549_510));
        assert_eq!(rated.imdb_rating, Some(4.2));
        assert_eq!(rated.imdb_votes, Some(5_000));
        assert_eq!(rated.effective_rating, Some(4.2));

        // A series rating of zero is "no rating", so only the IMDb value can win.
        let unrated = series_props(&items[1]);
        assert_eq!(unrated.effective_rating, None);
    }

    #[test]
    fn non_positive_provider_ratings_are_treated_as_missing() {
        let zero = [video_item("A", Some(0.0))];
        let negative = [video_item("A", Some(-1.0))];
        let nan = [video_item("A", Some(f64::NAN))];

        for mut items in [zero, negative, nan] {
            apply_imdb_ratings(&mut items, None, &enabled_config(0, ImdbRatingSource::ImdbThenProvider));
            assert_eq!(video_props(&items[0]).effective_rating, None);
        }
    }

    #[test]
    fn live_and_episode_items_are_untouched() {
        let mut lookup = test_lookup(&[(6_549_510, 42, 5_000)]);
        let mut items = [live_item("Channel [tt6549510]")];

        apply_imdb_ratings(&mut items, Some(&mut lookup), &enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        match items[0].header.additional_properties.as_ref() {
            Some(StreamProperties::Live(live)) => assert_eq!(live.as_ref(), &LiveStreamProperties::default()),
            other => panic!("live properties changed: {other:?}"),
        }
    }

    #[test]
    fn runtime_reports_enabled_and_open_state() {
        let runtime = ImdbRatingRuntime::new(ImdbConfig::default(), None);
        assert!(!runtime.is_enabled());
        assert!(!runtime.is_opened());
        assert!(!runtime.is_open_attempted());

        let enabled = ImdbRatingRuntime::new(enabled_config(0, ImdbRatingSource::ImdbThenProvider), None);
        assert!(enabled.is_enabled());
        assert!(!enabled.is_opened());

        let loaded = ImdbRatingRuntime::new(
            enabled_config(0, ImdbRatingSource::ImdbThenProvider),
            Some(ImdbLookup::from_rows(test_rows(&[(1, 10, 1)]))),
        );
        assert!(loaded.is_enabled());
        assert!(loaded.is_opened());
    }

    #[tokio::test]
    async fn live_only_run_does_not_open_the_table() {
        let temp = tempfile::tempdir().expect("tempdir");
        // No table is stored: an open would warn. Not opening is the observable.
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, "", true);
        let runtime = ImdbRatingRuntime::for_run(app_config, enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        let mut items = [live_item("Channel [tt6549510]")];
        runtime.ensure_open(&items).await;
        runtime.apply(&mut items);

        assert!(!runtime.is_opened(), "a live-only batch must not open the table");
        assert!(!runtime.is_open_attempted(), "a live-only batch must not even try to open the table");
    }

    #[tokio::test]
    async fn table_is_opened_once_across_batches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let path = get_imdb_ratings_path(storage_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create storage dir");
        }
        store_test_tree(&path, &[(6_549_510, 42, 5_000)]);

        let app_config = make_test_app_config(storage_dir, "", true);
        let runtime = ImdbRatingRuntime::for_run(app_config, enabled_config(0, ImdbRatingSource::ImdbThenProvider));

        let mut first = [video_item("Kill Em All 2 imdb=tt6549510", Some(6.8))];
        runtime.ensure_open(&first).await;
        assert!(runtime.is_opened());
        runtime.apply(&mut first);
        assert_eq!(video_props(&first[0]).imdb_rating, Some(4.2));

        // Removing the file proves the second batch reads the table opened by the first.
        std::fs::remove_file(&path).expect("remove table");

        let mut second = [video_item("Kill Em All 2 imdb=tt6549510", None)];
        runtime.ensure_open(&second).await;
        runtime.apply(&mut second);
        assert!(runtime.is_opened());
        assert_eq!(video_props(&second[0]).imdb_rating, Some(4.2));
    }

    #[tokio::test]
    async fn disabled_runtime_never_opens_the_table() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        store_test_tree(&get_imdb_ratings_path(storage_dir), &[(1_111_111, 42, 5_000)]);

        let app_config = make_test_app_config(storage_dir, "", false);
        let runtime = ImdbRatingRuntime::for_run(app_config, ImdbConfig::default());

        let mut items = [video_item("A [tt1111111]", Some(6.8))];
        runtime.ensure_open(&items).await;
        runtime.apply(&mut items);

        assert!(!runtime.is_opened());
        assert!(!runtime.is_open_attempted());
        assert_eq!(video_props(&items[0]).effective_rating, Some(6.8));
    }

    #[test]
    fn disk_lookup_applies_ratings_during_a_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("imdb_ratings.db");
        store_test_tree(&path, &[(6_549_510, 42, 5_000)]);

        let lookup = ImdbLookup::open(&path, true).expect("open disk table").expect("table exists");
        assert!(matches!(lookup, ImdbLookup::Disk { .. }));
        let runtime = ImdbRatingRuntime::new(enabled_config(1_000, ImdbRatingSource::ImdbThenProvider), Some(lookup));

        let mut items = [video_item("Kill Em All 2 imdb=tt6549510", Some(6.8))];
        runtime.apply(&mut items);

        let props = video_props(&items[0]);
        assert_eq!(props.imdb, Some(6_549_510));
        assert_eq!(props.imdb_rating, Some(4.2));
        assert_eq!(props.imdb_votes, Some(5_000));
        assert_eq!(props.effective_rating, Some(4.2));
    }

    fn seed_table(storage_dir: &str, rows: &[(u32, u16, u32)]) {
        let path = get_imdb_ratings_path(storage_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create storage dir");
        }
        store_test_tree(&path, rows);
    }

    fn stored_lookup(storage_dir: &str, disk_based: bool) -> ImdbLookup {
        let path = get_imdb_ratings_path(storage_dir);
        ImdbLookup::open(&path, disk_based).expect("open stored table").expect("table is stored")
    }

    #[tokio::test]
    async fn test_stream_and_parse_gzip_across_split_magic_header_chunks() {
        let tsv_content = "\
tconst\taverageRating\tnumVotes
tt0000001\t5.7\t2112
tt0000002\t8.1\t1500
tt0000003\t7.0\t3000
";
        let gzip_body = compress_to_gzip(tsv_content);
        assert!(gzip_body.len() >= 2);
        assert_eq!(gzip_body[0], 0x1f);
        assert_eq!(gzip_body[1], 0x8b);

        let chunk1 = std::io::Cursor::new(vec![gzip_body[0]]);
        let chunk2 = std::io::Cursor::new(gzip_body[1..].to_vec());
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(chunk1), Ok::<_, std::io::Error>(chunk2)]);
        let reader = tokio_util::io::StreamReader::new(stream);
        let buf_reader = tokio::io::BufReader::new(reader);

        let (tree, stats) = stream_and_parse_gzip(buf_reader).await.expect("parse split gzip stream");
        assert_eq!(stats.skipped_rows, 0);
        assert_eq!(stats.duplicate_rows, 0);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.query(&1).map(|e| (e.rating_x10, e.votes)), Some((57, 2112)));
        assert_eq!(tree.query(&2).map(|e| (e.rating_x10, e.votes)), Some((81, 1500)));
        assert_eq!(tree.query(&3).map(|e| (e.rating_x10, e.votes)), Some((70, 3000)));
    }

    #[tokio::test]
    async fn test_download_from_local_test_server_success() {
        let tsv_content = "\
tconst\taverageRating\tnumVotes
tt0000001\t5.7\t2112
tt0000002\t8.1\t1500
tt0000003\t7.0\t3000
";
        let gzip_body = compress_to_gzip(tsv_content);
        let (server_url, _handle) = start_test_server(gzip_body, 200).await;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
        let client = reqwest::Client::new();

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(result.is_ok(), "update should succeed: {:?}", result.err());
        assert_eq!(result.expect("row count"), 3);

        let mut memory = stored_lookup(storage_dir, false);
        assert_eq!(memory.lookup(1), Some(ImdbRating { rating: 5.7, votes: 2112 }));
        assert_eq!(memory.lookup(2), Some(ImdbRating { rating: 8.1, votes: 1500 }));
        assert_eq!(memory.lookup(3), Some(ImdbRating { rating: 7.0, votes: 3000 }));

        let mut disk = stored_lookup(storage_dir, true);
        assert_eq!(disk.lookup(1), Some(ImdbRating { rating: 5.7, votes: 2112 }));
        assert_eq!(disk.lookup(3), Some(ImdbRating { rating: 7.0, votes: 3000 }));
        assert_eq!(disk.lookup(4), None);
    }

    #[tokio::test]
    async fn test_failed_download_keeps_old_table() {
        let (server_url, _handle) = start_test_server(b"Internal Error".to_vec(), 500).await;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
        let client = reqwest::Client::new();

        seed_table(storage_dir, &[(10, 75, 100), (20, 85, 200)]);

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::DownloadFailed(_))));

        let mut lookup = stored_lookup(storage_dir, false);
        assert_eq!(lookup.lookup(10), Some(ImdbRating { rating: 7.5, votes: 100 }));
        assert_eq!(lookup.lookup(20), Some(ImdbRating { rating: 8.5, votes: 200 }));
    }

    #[tokio::test]
    async fn test_non_gzip_body_returns_error_and_keeps_old_table() {
        let plain_tsv = "tconst\taverageRating\tnumVotes\ntt0000001\t5.7\t2112\n";
        let (server_url, _handle) = start_test_server(plain_tsv.as_bytes().to_vec(), 200).await;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
        let client = reqwest::Client::new();

        seed_table(storage_dir, &[(10, 75, 100)]);

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::ProcessFailed(_))));

        let mut lookup = stored_lookup(storage_dir, false);
        assert_eq!(lookup.lookup(10), Some(ImdbRating { rating: 7.5, votes: 100 }));
    }

    #[tokio::test]
    async fn test_table_with_too_few_rows_rejected() {
        // The current table has 10 rows.
        let initial_rows: Vec<(u32, u16, u32)> = (1..=10).map(|id| (id, 70, 100)).collect();

        // The new table has only 2 rows (< 10 / 2 = 5 rows).
        let tsv_content = "\
tconst\taverageRating\tnumVotes
tt0000001\t5.7\t2112
tt0000002\t8.1\t1500
";
        let gzip_body = compress_to_gzip(tsv_content);
        let (server_url, _handle) = start_test_server(gzip_body, 200).await;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
        let client = reqwest::Client::new();

        seed_table(storage_dir, &initial_rows);

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::Rejected(_))));

        let path = get_imdb_ratings_path(storage_dir);
        assert_eq!(imdb_ratings_row_count(&path).expect("count stored rows"), 10);
    }

    #[tokio::test]
    async fn test_table_with_zero_rows_rejected() {
        let tsv_content = "tconst\taverageRating\tnumVotes\n";
        let gzip_body = compress_to_gzip(tsv_content);
        let (server_url, _handle) = start_test_server(gzip_body, 200).await;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, &format!("{server_url}/ratings.tsv.gz"), true);
        let client = reqwest::Client::new();

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::Rejected(_))));
        // A rejected table must not create a file.
        assert!(!get_imdb_ratings_path(storage_dir).exists());
    }

    #[tokio::test]
    async fn test_disabled_returns_disabled_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, "https://example.com/ratings.tsv.gz", false);
        let client = reqwest::Client::new();

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::Disabled)));
    }

    #[tokio::test]
    async fn test_already_running_returns_already_running_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_dir = temp.path().to_str().expect("path");
        let app_config = make_test_app_config(storage_dir, "https://example.com/ratings.tsv.gz", true);
        let ratings_path = get_imdb_ratings_path(storage_dir);
        if let Some(parent) = ratings_path.parent() {
            std::fs::create_dir_all(parent).expect("create storage dir");
        }
        let _file_lock = app_config.file_locks.write_lock(&ratings_path).await;
        let client = reqwest::Client::new();

        let result = update_imdb_ratings(&app_config, &client).await;
        assert!(matches!(result, Err(ImdbUpdateError::AlreadyRunning)));
    }
}
