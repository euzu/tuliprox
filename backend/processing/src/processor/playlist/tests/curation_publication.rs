//! Real HTTPS adapters -> eligible catalog -> target finalization -> disk/cache/watch publication.
//! DNS overrides and a fixture-only CA keep the production TMDB origin and certificate validation intact.
mod client_diagnostics;
mod item_limit;
mod tls_profile;
use super::{
    curation_effect_gate::{app_config, file_snapshot, processing_context},
    *,
};
use serde_json::json;
use shared::{
    model::{SeriesStreamProperties, StreamProperties, VideoStreamProperties},
    utils::hash_string,
};
use std::{collections::BTreeMap, sync::Mutex as StdMutex, time::Duration};
use tempfile::{tempdir, TempDir};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::{rustls, TlsAcceptor};
use tuliprox_repository::{load_m3u_target_storage, load_xtream_target_storage};

const MOVIES: &str = "/3/trending/movie/week?language=en-US&page=1";
const MOVIES_2: &str = "/3/trending/movie/week?language=en-US&page=2";
const TV: &str = "/3/trending/tv/day?language=en-US&page=1";
const TRAKT: &str = "/movies/popular?page=1&limit=100";
const MOVIE_PAGE: &str =
    r#"{"page":1,"total_pages":9,"total_results":99,"results":[{"id":8,"media_type":"movie"},{"id":8}]}"#;
const MOVIE_PAGE_2: &str = r#"{"page":2,"total_pages":2,"total_results":2,"results":[{"id":8},{"id":7}]}"#;
const TV_PAGE: &str = r#"{"page":1,"total_pages":1,"total_results":1,"results":[{"id":7,"media_type":"tv"}]}"#;
const TRAKT_PAGE: &str = r#"[{"title":"Unselected","ids":{"tmdb":99,"trakt":1,"slug":"unselected"}},{"title":"First","ids":{"tmdb":7,"trakt":2,"slug":"first"}}]"#;
const EMPTY: &str = r#"{"page":1,"total_pages":0,"total_results":0,"results":[]}"#;

struct DiscoveryServer {
    client: reqwest::Client,
    tmdb_client: reqwest::Client,
    certificate: reqwest::Certificate,
    address: std::net::SocketAddr,
    replies: Arc<StdMutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<StdMutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl DiscoveryServer {
    async fn start() -> Self {
        let cert =
            rcgen::generate_simple_self_signed(vec!["api.themoviedb.org".into(), "api.trakt.tv".into()]).unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_der(cert.cert.der()).unwrap())
            .resolve("api.themoviedb.org", address)
            .resolve("api.trakt.tv", address)
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let profile_directory = tempdir().unwrap();
        let tmdb_client =
            tuliprox_core::utils::network::request::create_tmdb_client(&app_config(profile_directory.path()))
                .unwrap()
                .no_proxy()
                .add_root_certificate(reqwest::Certificate::from_der(cert.cert.der()).unwrap())
                .resolve("api.themoviedb.org", address)
                .build()
                .unwrap();
        let replies = Arc::new(StdMutex::new(BTreeMap::from([
            (MOVIES.into(), response(200, MOVIE_PAGE)),
            (MOVIES_2.into(), response(200, MOVIE_PAGE_2)),
            (TV.into(), response(200, TV_PAGE)),
            (TRAKT.into(), response(200, TRAKT_PAGE)),
        ])));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let responses = Arc::clone(&replies);
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor.accept(tcp).await else { continue };
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0 && bytes.len() < 16_384, "bounded fixture request headers");
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let response = responses.lock().unwrap().get(path).cloned().unwrap_or_else(|| response(404, "{}"));
                recorded.lock().unwrap().push(request);
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            client,
            tmdb_client,
            certificate: reqwest::Certificate::from_der(cert.cert.der()).unwrap(),
            address,
            replies,
            requests,
            task,
        }
    }

    fn reply(&self, path: &str, status: u16, body: &str) { self.reply_raw(path, response(status, body)); }

    fn reply_raw(&self, path: &str, body: Vec<u8>) { self.replies.lock().unwrap().insert(path.into(), body); }
}

fn response(status: u16, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

impl Drop for DiscoveryServer {
    fn drop(&mut self) { self.task.abort(); }
}

fn catalog() -> Vec<PlaylistGroup> {
    let mut groups = Vec::new();
    for (cluster, rows) in [
        (XtreamCluster::Live, vec![(101, "Live", 0)]),
        (
            XtreamCluster::Video,
            vec![(201, "First", 7), (202, "Local alias", 7), (203, "Second", 8), (204, "Unselected", 99)],
        ),
        (XtreamCluster::Series, vec![(301, "Show", 7), (302, "Other show", 9)]),
    ] {
        let group = match cluster {
            XtreamCluster::Live => "Live",
            XtreamCluster::Video => "Movies",
            XtreamCluster::Series => "Series",
        };
        let mut channels = Vec::new();
        for (id, title, tmdb) in rows {
            let properties = match cluster {
                XtreamCluster::Live => None,
                XtreamCluster::Video => Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    tmdb: Some(tmdb),
                    ..Default::default()
                }))),
                XtreamCluster::Series => Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    tmdb: Some(tmdb),
                    ..Default::default()
                }))),
            };
            let item_type = match cluster {
                XtreamCluster::Live => PlaylistItemType::Live,
                XtreamCluster::Video => PlaylistItemType::Video,
                XtreamCluster::Series => PlaylistItemType::SeriesInfo,
            };
            let header = PlaylistItemHeader {
                id: id.to_string().intern(),
                uuid: hash_string(title),
                name: title.intern(),
                title: title.intern(),
                group: group.intern(),
                url: format!("http://media.invalid/{id}.mkv").intern(),
                item_type,
                xtream_cluster: cluster,
                epg_channel_id: (cluster == XtreamCluster::Live).then(|| "fixture.live".intern()),
                additional_properties: properties,
                ..Default::default()
            };
            channels.push(PlaylistItem { header });
            if cluster == XtreamCluster::Series {
                let title_episode = format!("{title} S01E01");
                channels.push(PlaylistItem {
                    header: PlaylistItemHeader {
                        id: (id + 100).to_string().intern(),
                        uuid: hash_string(&title_episode),
                        name: title_episode.as_str().intern(),
                        title: title_episode.as_str().intern(),
                        group: group.intern(),
                        item_type: PlaylistItemType::Series,
                        parent_code: hash_string(title).to_string().intern(),
                        xtream_cluster: cluster,
                        url: format!("http://media.invalid/{}.mkv", id + 100).intern(),
                        additional_properties: Some(StreamProperties::Episode(Box::new(
                            serde_json::from_value(json!({
                                "episode_id": id + 100, "episode": 1, "season": 1
                            }))
                            .unwrap(),
                        ))),
                        ..Default::default()
                    },
                });
            }
        }
        groups.push(PlaylistGroup {
            id: u32::try_from(groups.len() + 1).unwrap(),
            title: group.intern(),
            xtream_cluster: cluster,
            channels,
        });
    }
    groups
}

fn target(policy: &str, mixed: bool, xtream: bool) -> ConfigTargetDto {
    let mut value = json!({"name":"publication", "watch":[".*"], "use_memory_cache":true,
    "output":[{"type":"m3u","filename":"published.m3u"},{"type":"strm","directory":"strm","flat":true,"cleanup":true}],
    "curation":{"catalog_selection":policy,"tmdb":{"api":{"access_token":"fixture-discovery-token"},"trending":[
        {"kind":"movie","time_window":"week","limit":100,"create_xtream_category":xtream,"category_name":"TMDB Movies"},
        {"kind":"tv","time_window":"day","limit":100,"create_xtream_category":xtream,"category_name":"TMDB TV"}
    ]}}});
    if xtream {
        value["output"].as_array_mut().unwrap().push(json!({"type":"xtream"}));
    }
    if mixed {
        value["curation"]["trakt"] = json!({"api":{"api_key":"fixture-trakt-client"},"charts":[
            {"kind":"movies","chart":"popular","category_name":"Trakt Movies","tmdb_only":true}
        ]});
    }
    let mut dto: ConfigTargetDto = serde_json::from_value(value).unwrap();
    dto.prepare(1, None, None).unwrap();
    dto
}

struct Publication {
    directory: TempDir,
    context: Arc<PlaylistProcessingContext<shared::model::NoopSink>>,
    target: ConfigTarget,
    tmdb_client: reqwest::Client,
}

impl Publication {
    fn new(server: &DiscoveryServer, dto: &ConfigTargetDto) -> Self {
        let directory = tempdir().unwrap();
        let mut context = processing_context(app_config(directory.path()), Some(Arc::new(PlaylistStorageState::new())));
        context.client = server.client.clone();
        Self {
            directory,
            context: Arc::new(context),
            target: ConfigTarget::from(dto),
            tmdb_client: server.tmdb_client.clone(),
        }
    }

    async fn publish(&self) -> Result<(), Vec<TuliproxError>> { self.publish_catalog(catalog()).await }

    async fn publish_catalog(&self, playlist: Vec<PlaylistGroup>) -> Result<(), Vec<TuliproxError>> {
        let mut channel = shared::model::EpgChannel::new("fixture.live".intern());
        channel.title = Some("Fixture Live".intern());
        channel.programmes.push(shared::model::EpgProgramme::new_all(
            2_000_000_000,
            2_000_003_600,
            "fixture.live".intern(),
            playlist.first().and_then(|group| group.channels.first()).map(|item| Arc::clone(&item.header.title)),
            None,
            None,
        ));
        let prepared = PreparedTarget {
            target: self.target.clone(),
            playlist,
            epg: vec![tuliprox_core::model::Epg {
                priority: 0,
                logo_override: false,
                attributes: None,
                children: vec![Arc::new(channel)],
            }],
            processing: PipelineStats::default(),
            accepted_empty_clusters: ClusterFlags::empty(),
            library_empty: tuliprox_repository::LibraryEmptyPublication::None,
        };
        let (result, errors) = tokio::time::timeout(
            Duration::from_secs(5),
            target::finalize_prepared_target_with_tmdb(Arc::clone(&self.context), prepared, Some(&self.tmdb_client)),
        )
        .await
        .expect("bounded publication");
        assert!(errors.is_empty(), "unexpected non-curation errors: {errors:?}");
        result
    }

    async fn xtream_rows(&self) -> Vec<XtreamPlaylistItem> {
        let storage = load_xtream_target_storage(&self.context.config, &self.target).await.unwrap();
        storage
            .live
            .iter()
            .chain(storage.vod.iter())
            .chain(storage.series.iter())
            .map(|(_, item)| item.clone())
            .collect()
    }

    async fn cache_signature(&self) -> Vec<String> {
        let cache = self.context.playlist_state.as_ref().unwrap().data.read().await;
        let Some(storage) = cache.get(&self.target.name) else { return Vec::new() };
        let mut values = vec![format!(
            "target:{}:xtream={}:m3u={}:mapping={}",
            self.target.name,
            storage.xtream.is_some(),
            storage.m3u.is_some(),
            storage.id_mapping.is_some()
        )];
        if let Some(mapping) = &storage.id_mapping {
            values.extend(mapping.iter().map(|(key, record)| format!("mapping:{key:?}:{record:?}")));
        }
        if let Some(xtream) = &storage.xtream {
            values.extend(
                xtream
                    .live
                    .iter()
                    .chain(xtream.vod.iter())
                    .chain(xtream.series.iter())
                    .map(|(_, item)| format!("{item:?}")),
            );
        }
        if let Some(m3u) = &storage.m3u {
            values.extend(m3u.iter().map(|(_, item)| format!("{item:?}")));
        }
        values.sort();
        values
    }

    async fn identity_signature(&self) -> BTreeMap<String, (u32, u32)> {
        let cache = self.context.playlist_state.as_ref().unwrap().data.read().await;
        cache[&self.target.name]
            .id_mapping
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, record)| (record.uuid.to_string(), (record.virtual_id.get(), record.parent_virtual_id.get())))
            .collect()
    }

    fn strm_contents(&self) -> Vec<String> {
        file_snapshot(&self.directory.path().join("strm"))
            .into_iter()
            .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "strm"))
            .map(|(_, body)| String::from_utf8(body).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn tmdb_https_to_publication_preserves_rank_subjects_series_and_output_boundaries() {
    let server = DiscoveryServer::start().await;
    let run = Publication::new(&server, &target("curated", false, true));
    run.publish().await.unwrap();
    let rows = run.xtream_rows().await;
    let mut projected = rows.iter().filter(|item| item.group.as_ref() == "TMDB Movies").collect::<Vec<_>>();
    projected.sort_by_key(|item| item.source_ordinal);
    assert_eq!(
        projected.iter().map(|item| item.title.as_ref()).collect::<Vec<_>>(),
        ["Second", "First", "Local alias"]
    );
    assert!(rows.iter().any(|item| item.group.as_ref() == "TMDB TV" && item.title.as_ref() == "Show"));
    assert!(!rows.iter().any(|item| item.title.as_ref() == "Unselected" || item.title.starts_with("Other show")));
    let episode = rows
        .iter()
        .find(|item| item.group.as_ref() == "TMDB TV" && item.item_type == PlaylistItemType::Series)
        .expect("projected episode closure");
    let root = rows
        .iter()
        .find(|item| item.group.as_ref() == "TMDB TV" && item.item_type == PlaylistItemType::SeriesInfo)
        .unwrap();
    assert_eq!(
        episode.parent_code.as_ref(),
        hash_string(&format!("trakt-category:TMDB TV:{}", hash_string("Show"))).to_string()
    );
    {
        let cache = run.context.playlist_state.as_ref().unwrap().data.read().await;
        let mapping = cache[&run.target.name].id_mapping.as_ref().unwrap();
        let record =
            mapping.iter().map(|(_, record)| record).find(|record| record.virtual_id == episode.virtual_id).unwrap();
        assert_eq!(record.parent_virtual_id, root.virtual_id, "persisted episode points to the projected series");
    }
    let m3u = load_m3u_target_storage(&run.context.config, &run.target).await.unwrap();
    assert!(m3u.iter().all(|(_, item)| !item.group.starts_with("TMDB")));
    let text = std::fs::read_to_string(run.directory.path().join("published.m3u")).unwrap();
    for name in ["Live", "First", "Second", "Local alias", "Show S01E01"] {
        assert!(text.contains(name), "missing {name}");
    }
    assert!(!text.contains("Unselected") && !text.contains("Other show") && !text.contains("TMDB"));
    let strm = run.strm_contents();
    for id in [201, 202, 203, 401] {
        assert_eq!(strm.iter().filter(|body| body.contains(&format!("/{id}.mkv"))).count(), 1);
    }
    assert!(!strm.iter().any(|body| body.contains("/204.mkv") || body.contains("/402.mkv")));
    let before = run.identity_signature().await;
    run.publish().await.unwrap();
    assert_eq!(run.identity_signature().await, before, "repeat publication retains virtual IDs and aliases");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 6, "two movie pages and one TV page per run, no per-item fetch");
    assert!(requests.iter().all(|r| r.to_lowercase().contains("authorization: bearer fixture-discovery-token")));
}

#[tokio::test]
async fn mixed_https_publication_unions_subjects_without_duplicating_base_outputs_under_both_policies() {
    for policy in ["full", "curated"] {
        let server = DiscoveryServer::start().await;
        let run = Publication::new(&server, &target(policy, true, true));
        run.publish().await.unwrap();
        let rows = run.xtream_rows().await;
        assert!(rows.iter().any(|item| item.group.as_ref() == "Trakt Movies" && item.title.as_ref() == "Unselected"));
        assert!(rows.iter().any(|item| item.group.as_ref() == "TMDB Movies" && item.title.as_ref() == "First"));
        assert_eq!(rows.iter().any(|item| item.title.as_ref() == "Other show"), policy == "full");
        let m3u = load_m3u_target_storage(&run.context.config, &run.target).await.unwrap();
        for name in ["First", "Local alias", "Unselected"] {
            assert_eq!(m3u.iter().filter(|(_, item)| item.title.as_ref() == name).count(), 1, "union once: {name}");
        }
        assert!(m3u.iter().all(|(_, item)| !item.group.starts_with("TMDB") && !item.group.starts_with("Trakt")));
        assert_eq!(server.requests.lock().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn tmdb_https_selection_only_publishes_m3u_strm_without_an_xtream_output() {
    let server = DiscoveryServer::start().await;
    let run = Publication::new(&server, &target("curated", false, false));
    run.publish().await.unwrap();
    let text = std::fs::read_to_string(run.directory.path().join("published.m3u")).unwrap();
    assert!(text.contains("First") && text.contains("Show S01E01") && text.contains("Live"));
    assert!(!text.contains("Unselected") && !text.contains("TMDB"));
    assert!(!run.strm_contents().is_empty());
    let cache = run.context.playlist_state.as_ref().unwrap().data.read().await;
    assert!(cache[&run.target.name].xtream.is_none());
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn tmdb_https_empty_and_no_match_clear_published_vod_series_but_keep_live() {
    for body in
        [EMPTY, r#"{"page":1,"total_pages":1,"total_results":1,"results":[{"id":999,"title":"First","name":"Show"}]}"#]
    {
        let server = DiscoveryServer::start().await;
        let run = Publication::new(&server, &target("curated", false, true));
        run.publish().await.unwrap();
        assert!(!run.strm_contents().is_empty());
        server.reply(MOVIES, 200, body);
        server.reply(TV, 200, body);
        run.publish().await.unwrap();
        let rows = run.xtream_rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_ref(), "Live");
        let m3u = load_m3u_target_storage(&run.context.config, &run.target).await.unwrap();
        assert_eq!(m3u.len(), 1);
        let strm = run.strm_contents();
        assert_eq!(strm.len(), 1, "Live survives; only VOD and series files are cleared");
        assert!(strm[0].contains("/101.mkv"));
        let watch: std::collections::BTreeSet<Arc<str>> = tuliprox_core::utils::binary_deserialize(
            &std::fs::read(run.directory.path().join("publication.groups.bin")).unwrap(),
        )
        .unwrap();
        assert_eq!(watch.iter().map(AsRef::as_ref).collect::<Vec<&str>>(), ["Live"]);
        let cache = run.context.playlist_state.as_ref().unwrap().data.read().await;
        let xtream = cache[&run.target.name].xtream.as_ref().unwrap();
        assert!(xtream.vod.is_empty() && xtream.series.is_empty());
    }
}

#[tokio::test]
async fn either_https_failure_retains_every_published_file_id_cache_and_watch_in_full_and_curated() {
    for policy in ["full", "curated"] {
        for (route, status, body) in [
            (MOVIES, 503, "{}"),
            (TRAKT, 503, "{}"),
            (MOVIES, 200, r#"{"page":1,"total_pages":1,"total_results":1,"results":[{"id":0}]}"#),
        ] {
            let server = DiscoveryServer::start().await;
            let run = Publication::new(&server, &target(policy, true, true));
            let initial = file_snapshot(run.directory.path());
            server.reply(route, status, body);
            assert!(run.publish().await.is_err(), "first run must not publish a successful sibling");
            assert_eq!(file_snapshot(run.directory.path()), initial);
            assert!(run.cache_signature().await.is_empty());
            server.reply(route, 200, if route == TRAKT { TRAKT_PAGE } else { MOVIE_PAGE });
            run.publish().await.unwrap();
            let files = file_snapshot(run.directory.path());
            let cache = run.cache_signature().await;
            assert!(!files.is_empty() && !cache.is_empty());
            server.reply(route, status, body);
            assert!(run.publish().await.is_err());
            assert_eq!(file_snapshot(run.directory.path()), files);
            assert_eq!(run.cache_signature().await, cache);
        }
    }
}

#[tokio::test]
async fn legacy_to_canonical_trakt_migration_retains_published_virtual_ids_and_aliases() {
    let server = DiscoveryServer::start().await;
    let mut canonical = target("curated", true, true);
    canonical.curation.as_mut().unwrap().tmdb = None;
    let mut legacy = canonical.clone();
    let curation = legacy.curation.take().unwrap();
    let mut value = serde_json::to_value(curation.trakt.unwrap()).unwrap();
    value["catalog_selection"] = serde_json::to_value(curation.catalog_selection).unwrap();
    for output in &mut legacy.output {
        if let TargetOutputDto::Xtream(xtream) = output {
            xtream.trakt = Some(serde_json::from_value(value.clone()).unwrap());
        }
    }
    legacy.prepare(1, None, None).unwrap();
    let mut run = Publication::new(&server, &legacy);
    run.publish().await.unwrap();
    let before = run.identity_signature().await;
    run.target = ConfigTarget::from(&canonical);
    run.publish().await.unwrap();
    assert_eq!(run.identity_signature().await, before);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}
