mod client;
mod errors;
mod model;

use crate::kernel::{
    curate_category, CuratedMediaReference, CurationCategorySpec, CurationMatchPolicy, CurationMediaScope,
    ProjectionIdentityStrategy,
};
use client::TraktClient;
use log::{debug, info, warn};
use model::TraktListItem;
use shared::model::{PlaylistGroup, TraktContentType};
use tuliprox_core::model::{TraktChartConfig, TraktConfig, TraktListConfig};

// Compatibility policy for the current Xtream projection. This namespace is
// deliberately supplied by the adapter rather than treated as canonical media identity.
const LEGACY_TRAKT_CATEGORY_NAMESPACE: &str = "trakt-category";

/// Curate virtual playlist categories from the configured Trakt lists and charts.
///
/// Disabled or source-less configuration is a no-op. Individual list/chart
/// failures are logged and isolated so successful sources still contribute.
pub async fn curate_trakt_categories(
    http_client: &reqwest::Client,
    playlist: &[PlaylistGroup],
    target_name: &str,
    trakt_config: &TraktConfig,
) -> Option<Vec<PlaylistGroup>> {
    if !trakt_config.enabled {
        return None;
    }
    if trakt_config.lists.is_empty() && trakt_config.charts.is_empty() {
        debug!("No Trakt lists or charts configured for target {target_name}");
        return None;
    }

    let processor = match TraktCategoriesProcessor::new(http_client, trakt_config) {
        Ok(processor) => processor,
        Err(error) => {
            warn!("Skipping Trakt curation for target '{target_name}': {}", error.message());
            return None;
        }
    };

    Some(processor.process(playlist, target_name, trakt_config).await)
}

struct TraktCategoriesProcessor {
    client: TraktClient,
}

impl TraktCategoriesProcessor {
    fn new(http_client: &reqwest::Client, trakt_config: &TraktConfig) -> Result<Self, shared::error::TuliproxError> {
        let client = TraktClient::new(http_client.clone(), trakt_config.api.clone())?;
        Ok(Self { client })
    }

    async fn process(
        &self,
        playlist: &[PlaylistGroup],
        target_name: &str,
        trakt_config: &TraktConfig,
    ) -> Vec<PlaylistGroup> {
        info!(
            "Processing {} Trakt lists and {} Trakt charts for target {target_name}",
            trakt_config.lists.len(),
            trakt_config.charts.len()
        );
        let mut new_categories = Vec::new();
        let mut total_matches = 0;

        for list_config in &trakt_config.lists {
            let source_label = format!("{}:{}", list_config.user, list_config.list_slug);
            let specification = list_category_spec(list_config);

            match self.client.get_list_items(list_config).await {
                Ok(items) => {
                    debug!("Processing Trakt list {source_label} with {} items", items.len());
                    let references = translate_items(items);
                    append_categories(&references, playlist, &specification, &mut new_categories, &mut total_matches);
                }
                Err(error) => warn!("Failed to fetch Trakt list {source_label}: {}", error.message()),
            }
        }

        for chart_config in &trakt_config.charts {
            let source_label = format!("{}:{}", chart_config.kind, chart_config.chart);
            let specification = chart_category_spec(chart_config);

            match self.client.get_chart_items(chart_config).await {
                Ok(items) => {
                    debug!("Processing Trakt chart {source_label} with {} items", items.len());
                    let references = translate_items(items);
                    append_categories(&references, playlist, &specification, &mut new_categories, &mut total_matches);
                }
                Err(error) => warn!("Failed to fetch Trakt chart {source_label}: {}", error.message()),
            }
        }

        info!(
            "Trakt processing complete: created {} categories with {total_matches} total matches",
            new_categories.len()
        );
        new_categories
    }
}

fn append_categories(
    references: &[CuratedMediaReference],
    playlist: &[PlaylistGroup],
    specification: &CurationCategorySpec<'_>,
    categories: &mut Vec<PlaylistGroup>,
    total_matches: &mut usize,
) {
    for category in curate_category(references, playlist, specification) {
        if !category.channels.is_empty() {
            *total_matches += category.channels.len();
            let category_len = category.channels.len();
            categories.push(category);
            debug!("Created Trakt category '{}' with {category_len} items", specification.name);
        }
    }
}

fn translate_items(items: Vec<TraktListItem>) -> Vec<CuratedMediaReference> {
    items.into_iter().filter_map(TraktListItem::into_curated_reference).collect()
}

fn list_category_spec(config: &TraktListConfig) -> CurationCategorySpec<'_> {
    category_spec(&config.category_name, config.content_type, config.tmdb_only, config.fuzzy_match_threshold)
}

fn chart_category_spec(config: &TraktChartConfig) -> CurationCategorySpec<'_> {
    category_spec(&config.category_name, config.kind.content_type(), config.tmdb_only, config.fuzzy_match_threshold)
}

fn category_spec(
    category_name: &str,
    content_type: TraktContentType,
    tmdb_only: bool,
    fuzzy_match_threshold: u8,
) -> CurationCategorySpec<'_> {
    let media_scope = match content_type {
        TraktContentType::Vod => CurationMediaScope::Movies,
        TraktContentType::Series => CurationMediaScope::Series,
        TraktContentType::Both => CurationMediaScope::Both,
    };
    let match_policy = if tmdb_only {
        CurationMatchPolicy::ExactTmdbOnly
    } else {
        CurationMatchPolicy::ExactTmdbThenFuzzy { threshold_percent: fuzzy_match_threshold }
    };
    CurationCategorySpec {
        name: category_name,
        media_scope,
        match_policy,
        projection_identity: ProjectionIdentityStrategy::LegacyCategoryScoped {
            namespace: LEGACY_TRAKT_CATEGORY_NAMESPACE,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trakt::model::{TraktIds, TraktMovie, TraktShow};
    use shared::{
        model::{
            EpisodeStreamProperties, FieldGet, HeaderField, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
            SeriesStreamProperties, StreamProperties, TraktChartKind, TraktChartType, VideoStreamProperties, VirtualId,
            XtreamCluster,
        },
        utils::{hash_string, Internable},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };
    use tuliprox_core::model::TraktApiConfig;

    #[tokio::test]
    async fn configured_curation_without_a_usable_client_id_makes_no_request() {
        for client_id in ["", " \t\r\n ", "sensitive-client-id\ninjected-header"] {
            let requests = Arc::new(AtomicUsize::new(0));
            let (base_url, server) = spawn_counting_trakt_server(Arc::clone(&requests)).await;
            let config = trakt_config(client_id, base_url, true, vec![remote_list_config("Missing")], Vec::new());

            let result = curate_trakt_categories(&reqwest::Client::new(), &[], "test-target", &config).await;

            assert!(result.is_none());
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn disabled_or_source_less_configuration_makes_no_request() {
        let requests = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_counting_trakt_server(Arc::clone(&requests)).await;
        let disabled = trakt_config("", base_url.clone(), false, vec![remote_list_config("Disabled")], Vec::new());
        let source_less = trakt_config("", base_url, true, Vec::new(), Vec::new());

        assert!(curate_trakt_categories(&reqwest::Client::new(), &[], "test-target", &disabled).await.is_none());
        assert!(curate_trakt_categories(&reqwest::Client::new(), &[], "test-target", &source_less).await.is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn failed_list_does_not_suppress_successful_chart() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (base_url, server) = spawn_partial_success_trakt_server(Arc::clone(&requests)).await;
        let config = trakt_config(
            "test-client-id",
            base_url,
            true,
            vec![remote_list_config("Unavailable List")],
            vec![remote_chart_config("Available Chart")],
        );
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![video_item("Movie 1", Some(11))],
            xtream_cluster: XtreamCluster::Video,
        }];

        let categories = curate_trakt_categories(&reqwest::Client::new(), &playlist, "test-target", &config)
            .await
            .expect("configured sources should produce a result");

        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].title.as_ref(), "Available Chart");
        assert_eq!(categories[0].channels.len(), 1);
        assert_eq!(categories[0].channels[0].header.title.as_ref(), "Movie 1");
        assert_eq!(requests.lock().expect("requests").len(), 2);
        server.await.expect("test server should finish");
    }

    #[test]
    fn raw_records_are_translated_before_matching() {
        let references = translate_items(vec![trakt_list_movie("The Smashing Machine", Some(2025), Some(760_329), 7)]);

        assert_eq!(references.len(), 1);
        assert_eq!(references[0].kind, crate::kernel::CurationMediaKind::Movie);
        assert_eq!(references[0].title, "The Smashing Machine");
        assert_eq!(references[0].year, Some(2025));
        assert_eq!(references[0].tmdb_id, Some(760_329));
        assert_eq!(references[0].rank, Some(7));
    }

    #[test]
    fn legacy_trakt_projection_identity_is_exact_and_category_scoped() {
        let mut source_item = video_item("The Smashing Machine", Some(760_329));
        source_item.header.uuid = hash_string("curation-source-item");
        assert_eq!(
            source_item.header.uuid.to_string(),
            "e2f49417a9bc5e05942ee77996a18ce27d2445d27a331cfd3417f11448b886e1"
        );
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![source_item],
            xtream_cluster: XtreamCluster::Video,
        }];
        let references = translate_items(vec![trakt_list_movie("The Smashing Machine", Some(2025), Some(760_329), 1)]);
        let featured_config = remote_list_config("Featured");
        let renoir_config = remote_list_config("Renoir");

        let featured = curate_category(&references, &playlist, &list_category_spec(&featured_config));
        let renoir = curate_category(&references, &playlist, &list_category_spec(&renoir_config));
        let featured_item = &featured[0].channels[0];
        let renoir_item = &renoir[0].channels[0];

        assert_eq!(featured_item.header.group.as_ref(), "Featured");
        assert_eq!(renoir_item.header.group.as_ref(), "Renoir");
        assert_eq!(
            featured_item.header.uuid.to_string(),
            "1a3fda78972a6e368ac159094c5b4d5b722630bdc7021530624ff0971c45092b"
        );
        assert_ne!(featured_item.header.uuid, renoir_item.header.uuid);
    }

    #[test]
    fn legacy_trakt_series_projection_preserves_exact_child_identity_and_parent_linkage() {
        let mut series = series_item("Slow Horses", Some(12345));
        series.header.uuid = hash_string("series-source-item");
        let source_parent_code = series.header.uuid.intern();
        let mut episode = episode_item("Old Scores", &source_parent_code, 7001);
        episode.header.uuid = hash_string("episode-source-item");
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![series, episode],
            xtream_cluster: XtreamCluster::Series,
        }];
        let references = translate_items(vec![trakt_list_show("Slow Horses", Some(2022), Some(12345), 1)]);
        let config = TraktListConfig { content_type: TraktContentType::Series, ..remote_list_config("Trending") };

        let categories = curate_category(&references, &playlist, &list_category_spec(&config));
        let cloned_series = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::SeriesInfo)
            .expect("series info clone");
        let cloned_episode = categories[0]
            .channels
            .iter()
            .find(|item| item.header.item_type == PlaylistItemType::Series)
            .expect("episode clone");

        assert_eq!(
            cloned_series.header.uuid.to_string(),
            "57431046882ed9f79d64a5ffdb311231389216e85f0704911104d7926def2cb3"
        );
        assert_eq!(
            cloned_episode.header.uuid.to_string(),
            "12557067723f9dd4b74b3a845d20af2e8be18bdd6a49bf9095fb1be70c7a8cf1"
        );
        assert_eq!(cloned_episode.header.parent_code, cloned_series.header.uuid.intern());
    }

    #[test]
    fn quality_caption_behavior_survives_trakt_translation() {
        let mut item = video_item("Clean Title", Some(1));
        item.header.group = "Provider UHD".intern();
        let playlist = vec![PlaylistGroup {
            id: 1,
            title: "Original".intern(),
            channels: vec![item],
            xtream_cluster: XtreamCluster::Video,
        }];
        let references = translate_items(vec![trakt_list_movie("Clean Title", None, Some(1), 1)]);

        let categories = curate_category(&references, &playlist, &list_category_spec(&remote_list_config("Featured")));

        assert_eq!(
            categories[0].channels[0].header.get(HeaderField::Caption).expect("quality caption").as_cow(),
            "[UHD] Clean Title"
        );
    }

    async fn spawn_counting_trakt_server(requests: Arc<AtomicUsize>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let _ = read_request(&mut stream).await;
                requests.fetch_add(1, Ordering::SeqCst);
                write_response(&mut stream, "200 OK", "[]").await;
            }
        });
        (format!("http://{addr}"), server)
    }

    async fn spawn_partial_success_trakt_server(requests: Arc<Mutex<Vec<String>>>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept test request");
                let request = read_request(&mut stream).await;
                let is_list_request = request.contains("/users/test-user/lists/test-list/items");
                requests.lock().expect("requests").push(request);
                if is_list_request {
                    write_response(&mut stream, "403 Forbidden", "response body must not affect the next source").await;
                } else {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"[{"title":"Movie 1","year":2026,"ids":{"trakt":1,"slug":"movie-1","tvdb":null,"imdb":null,"tmdb":11,"tvrage":null}}]"#,
                    )
                    .await;
                }
            }
        });
        (format!("http://{addr}"), server)
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request_bytes = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let read = stream.read(&mut buffer).await.expect("read request");
            if read == 0 {
                break;
            }
            request_bytes.extend_from_slice(&buffer[..read]);
            if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&request_bytes).to_string()
    }

    async fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.expect("write response");
    }

    fn trakt_config(
        client_id: &str,
        url: String,
        enabled: bool,
        lists: Vec<TraktListConfig>,
        charts: Vec<TraktChartConfig>,
    ) -> TraktConfig {
        TraktConfig {
            enabled,
            api: TraktApiConfig {
                api_key: client_id.to_string(),
                version: "2".to_string(),
                url,
                user_agent: "tuliprox-test".to_string(),
            },
            lists,
            charts,
        }
    }

    fn remote_list_config(category_name: &str) -> TraktListConfig {
        TraktListConfig {
            user: "test-user".to_string(),
            list_slug: "test-list".to_string(),
            category_name: category_name.to_string(),
            content_type: TraktContentType::Vod,
            tmdb_only: true,
            fuzzy_match_threshold: 100,
        }
    }

    fn remote_chart_config(category_name: &str) -> TraktChartConfig {
        TraktChartConfig {
            kind: TraktChartKind::Movies,
            chart: TraktChartType::Popular,
            category_name: category_name.to_string(),
            tmdb_only: true,
            fuzzy_match_threshold: 100,
        }
    }

    fn video_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Video,
                item_type: PlaylistItemType::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..VideoStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn series_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: format!("series-{title}").intern(),
                input_name: "input".intern(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://unavailable/server/shows/{title}").intern(),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::SeriesInfo,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: title.intern(),
                    tmdb,
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn episode_item(title: &str, parent_code: &Arc<str>, virtual_id: u32) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                uuid: hash_string(&format!("episode:{title}:{virtual_id}")),
                id: format!("episode-{virtual_id}").intern(),
                input_name: "input".intern(),
                parent_code: parent_code.clone(),
                title: title.intern(),
                name: title.intern(),
                url: format!("media-server://plex/server/{virtual_id}?part_key=%2Flibrary%2Fparts%2Fredacted").intern(),
                virtual_id: VirtualId::new(virtual_id),
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::Series,
                additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                    episode_id: virtual_id,
                    episode: 1,
                    season: 1,
                    added: None,
                    release_date: None,
                    series_release_date: None,
                    tmdb: None,
                    movie_image: "".intern(),
                    container_extension: "mkv".intern(),
                    video: None,
                    audio: None,
                    plot: None,
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn trakt_list_movie(title: &str, year: Option<u32>, tmdb_id: Option<u32>, rank: u32) -> TraktListItem {
        TraktListItem {
            id: u64::from(rank),
            rank: Some(rank),
            listed_at: String::new(),
            notes: None,
            item_type: "movie".to_string(),
            movie: Some(TraktMovie { ids: trakt_ids(title, tmdb_id, rank), title: title.to_string(), year }),
            show: None,
        }
    }

    fn trakt_list_show(title: &str, year: Option<u32>, tmdb_id: Option<u32>, rank: u32) -> TraktListItem {
        TraktListItem {
            id: u64::from(rank),
            rank: Some(rank),
            listed_at: String::new(),
            notes: None,
            item_type: "show".to_string(),
            movie: None,
            show: Some(TraktShow { ids: trakt_ids(title, tmdb_id, rank), title: title.to_string(), year }),
        }
    }

    fn trakt_ids(title: &str, tmdb_id: Option<u32>, trakt_id: u32) -> TraktIds {
        TraktIds { trakt: trakt_id, slug: title.to_string(), tvdb: None, imdb: None, tmdb: tmdb_id, tvrage: None }
    }
}
