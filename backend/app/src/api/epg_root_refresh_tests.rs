use super::{
    internal_csv::csv_patch_batch_update_credentials,
    model::{create_test_app_state, AppState},
    source_yml_patch::{
        execute_account_source_patches, merge_runtime_document, runtime_patch_document, SourcesYmlPatch,
    },
};
use crate::model::{Config, ConfigInput, SourcesConfig};
use shared::model::{
    ConfigInputDto, EpgConfigDto, EpgSmartMatchConfigDto, EpgSourceDto, EpgSourceTypeDto, IcsEpgSourceConfigDto,
    InputType, SourcesConfigDto,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tuliprox_processing::processor::PlaylistProcessingContext;

struct RecordingServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl RecordingServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("local EPG listener");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let len = socket.read(&mut chunk).await.unwrap();
                    assert!(len > 0, "incomplete request");
                    bytes.extend_from_slice(&chunk[..len]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let target = request.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_string();
                let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
                let username = url
                    .query_pairs()
                    .find(|(key, _)| key == "username")
                    .map_or_else(|| "explicit".into(), |(_, value)| value.into_owned());
                received.lock().unwrap().push(target);
                // The Web-UI XMLTV parser drops programmes that ended before yesterday midnight and then
                // drops channels left without programmes, so the fixture must stay inside that window.
                let now = chrono::Utc::now();
                let start = (now - chrono::Duration::hours(1)).format("%Y%m%d%H%M%S +0000");
                let stop = (now + chrono::Duration::hours(1)).format("%Y%m%d%H%M%S +0000");
                let body = format!("<?xml version=\"1.0\"?><tv><channel id=\"{username}\"><display-name>{username}</display-name></channel><programme channel=\"{username}\" start=\"{start}\" stop=\"{stop}\"><title>{username}</title></programme></tv>");
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self { base, requests, task }
    }

    fn rotation_boundary(&self) -> usize {
        let boundary = self.requests.lock().unwrap().len();
        eprintln!("Rotation boundary: {boundary} completed requests");
        boundary
    }

    fn assert_requests_since(&self, boundary: usize, expected: &[&str]) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(&requests[boundary..], expected);
        eprintln!("Requests since rotation boundary {boundary}: {:?}", &requests[boundary..]);
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) { self.task.abort(); }
}

#[derive(Clone, Copy, Debug)]
enum Persistence {
    Yaml,
    Csv,
}

#[derive(Clone, Copy)]
enum SourceSet {
    Auto,
    Mixed,
    Collision,
    Missing,
    Explicit,
}

struct Fixture {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
    path: PathBuf,
    persistence: Persistence,
    server: RecordingServer,
    original: String,
    explicit_sources: Vec<crate::model::EpgSource>,
}

impl Fixture {
    #[allow(clippy::too_many_lines)]
    async fn new(persistence: Persistence, hot_reload: bool, source_set: SourceSet) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let server = RecordingServer::start().await;
        let auto = EpgSourceDto { url: "AuTo".into(), priority: -2, logo_override: true, ..Default::default() };
        let old_url = format!("{}/xmltv.php?username=old-user&password=old-pass", server.base);
        let explicit = EpgSourceDto { url: old_url, priority: 4, logo_override: false, ..Default::default() };
        let local = temp.path().join("explicit.xml");
        tokio::fs::write(&local, "<?xml version=\"1.0\"?><tv/>").await.unwrap();
        let sources = match source_set {
            SourceSet::Auto => vec![auto],
            SourceSet::Collision => vec![auto, explicit],
            SourceSet::Missing => Vec::new(),
            SourceSet::Explicit => vec![explicit],
            SourceSet::Mixed => vec![
                auto,
                explicit,
                EpgSourceDto {
                    url: "https://explicit.example/guide.xml?username=old-user&password=old-pass".into(),
                    priority: 5,
                    logo_override: true,
                    ..Default::default()
                },
                EpgSourceDto { url: "provider://same-provider/explicit.xml".into(), priority: 7, ..Default::default() },
                EpgSourceDto { url: local.to_string_lossy().into_owned(), priority: 9, ..Default::default() },
                EpgSourceDto {
                    source_type: EpgSourceTypeDto::Ics,
                    url: format!("{}/calendar.ics", server.base),
                    priority: 11,
                    logo_override: true,
                    channel_id: Some("calendar.channel".into()),
                    channel_title: Some("Calendar channel".into()),
                    match_names: vec!["Calendar".into()],
                    ics: Some(IcsEpgSourceConfigDto::default()),
                },
            ],
        };
        let path = temp.path().join(match persistence {
            Persistence::Yaml => "source.yml",
            Persistence::Csv => "accounts.csv",
        });
        let mut dto = ConfigInputDto {
            id: 1,
            name: "logical-input".into(),
            input_type: InputType::Xtream,
            url: server.base.clone(),
            username: Some("old-user".into()),
            password: Some("old-pass".into()),
            cache_duration_seconds: 3600,
            epg: if matches!(source_set, SourceSet::Missing) {
                None
            } else {
                Some(EpgConfigDto {
                    sources: Some(sources),
                    smart_match: Some(EpgSmartMatchConfigDto {
                        enabled: true,
                        fuzzy_matching: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            },
            ..Default::default()
        };
        match persistence {
            Persistence::Yaml => {
                let doc = SourcesConfigDto { inputs: vec![dto.clone()], ..Default::default() };
                tokio::fs::write(&path, serde_saphyr::to_string(&doc).unwrap()).await.unwrap();
                dto.prepare_epg(true).unwrap();
            }
            Persistence::Csv => {
                let base = &server.base;
                tokio::fs::write(&path, format!("#name;username;password;url;enabled;max_connections;priority;exp_date\nalias-row;alias-user;alias-pass;{base};0;1;0;\ncsv-root;old-user;old-pass;{base};1;1;0;\n")).await.unwrap();
                dto.input_type = InputType::XtreamBatch;
                dto.url = format!("batch://{}", path.display());
                dto.username = None;
                dto.password = None;
                let mut doc = SourcesConfigDto { inputs: vec![dto], ..Default::default() };
                crate::config_loader::prepare_sources_batch(&mut doc, true).await.unwrap();
                dto = doc.inputs.remove(0);
            }
        }
        let mut input = ConfigInput::from(&dto);
        input.prepare(&[]).unwrap();
        let explicit_sources = dto
            .epg
            .as_ref()
            .and_then(|epg| epg.sources.as_ref())
            .into_iter()
            .flatten()
            .filter(|source| !source.url.eq_ignore_ascii_case("auto"))
            .map(crate::model::EpgSource::from)
            .collect();
        let original = tokio::fs::read_to_string(&path).await.unwrap();
        let state = create_test_app_state(Config {
            config_hot_reload: hot_reload,
            storage_dir: temp.path().join("storage").to_string_lossy().into_owned(),
            backup_dir: Some(temp.path().join("backup").to_string_lossy().into_owned()),
            ..Default::default()
        });
        state.app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input)], ..Default::default() }));
        Self { _temp: temp, state, path, persistence, server, original, explicit_sources }
    }

    fn input(&self) -> Arc<ConfigInput> { Arc::clone(&self.state.app_config.sources.load().inputs[0]) }

    fn context(&self) -> PlaylistProcessingContext<shared::model::NoopSink> {
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        PlaylistProcessingContext::for_epg_test(Arc::clone(&self.state.app_config), client)
    }

    async fn download(
        &self,
        ctx: &PlaylistProcessingContext<shared::model::NoopSink>,
        input: &ConfigInput,
    ) -> Vec<String> {
        let storage = self.state.app_config.config.load().storage_dir.clone();
        let (guide, errors) = tuliprox_processing::epg::get_xmltv(ctx, input, None, &storage).await;
        assert!(errors.is_empty(), "EPG acquisition errors: {errors:?}");
        let Some(epg) = input.epg.as_ref() else {
            assert!(guide.is_none());
            return Vec::new();
        };
        let mut sources = guide.expect("acquired EPG guide").get_epg_sources().clone();
        sources.sort_by_key(|source| source.source_idx);
        assert_eq!(sources.len(), epg.sources.len());
        let mut contents = Vec::new();
        for source in sources {
            let channels = crate::model::parse_xmltv_for_web_ui_from_file(&source.file_path).await.unwrap();
            assert_eq!(channels.len(), 1, "valid XMLTV channel and programme");
            contents.push(tokio::fs::read_to_string(&source.file_path).await.unwrap());
        }
        contents
    }

    async fn rotate(&self, username: &str, password: &str) {
        match self.persistence {
            Persistence::Yaml => {
                execute_account_source_patches(
                    &self.state,
                    &self.path,
                    &[SourcesYmlPatch::UpdateRootCredentials {
                        input_name: "logical-input".into(),
                        username: username.into(),
                        password: password.into(),
                        exp_date: None,
                    }],
                )
                .await
                .unwrap();
            }
            Persistence::Csv => {
                let old = self.input();
                let guard = self.state.app_config.file_locks.write_lock(&self.path).await;
                csv_patch_batch_update_credentials(
                    &self.state,
                    &guard,
                    InputType::Xtream,
                    &self.path,
                    &Arc::from("csv-root"),
                    old.username.as_deref().unwrap(),
                    old.password.as_deref().unwrap(),
                    username,
                    password,
                    None,
                )
                .await
                .unwrap();
            }
        }
    }

    async fn assert_persistence(&self) {
        let content = tokio::fs::read_to_string(&self.path).await.unwrap();
        match self.persistence {
            Persistence::Yaml => {
                let original: SourcesConfigDto = serde_saphyr::from_str(&self.original).unwrap();
                let current: SourcesConfigDto = serde_saphyr::from_str(&content).unwrap();
                assert_eq!(current.inputs[0].epg, original.inputs[0].epg);
                assert_eq!(current.inputs[0].username, self.input().username);
                assert_eq!(current.inputs[0].password, self.input().password);
            }
            Persistence::Csv => {
                let parse = |text: &str| {
                    crate::repository::csv_read_inputs_from_reader(
                        InputType::Xtream,
                        crate::utils::EnvResolvingReader::new(std::io::BufReader::new(std::io::Cursor::new(
                            text.as_bytes(),
                        ))),
                    )
                    .unwrap()
                };
                let before = parse(&self.original);
                let after = parse(&content);
                assert_eq!(content.lines().next(), self.original.lines().next());
                assert_eq!(after.len(), before.len());
                assert_eq!(
                    after.iter().find(|row| row.name.as_ref() == "alias-row"),
                    before.iter().find(|row| row.name.as_ref() == "alias-row")
                );
                let root = after.iter().find(|row| row.name.as_ref() == "csv-root").unwrap();
                let input = self.input();
                assert_eq!(root.username, input.username);
                assert_eq!(root.password, input.password);
                assert_eq!(root.url, input.url);
                assert_eq!(input.t_batch_url, Some(format!("batch://{}", self.path.display())));
                assert_eq!(input.aliases.as_ref().unwrap().len(), 1);
                assert_eq!(input.aliases.as_ref().unwrap()[0].name.as_ref(), "alias-row");
            }
        }
    }

    fn assert_explicit_sources(&self) {
        let input = self.input();
        let sources = &input.epg.as_ref().unwrap().sources;
        for expected in &self.explicit_sources {
            let actual = sources.iter().find(|source| source.url == expected.url).expect("explicit source retained");
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        }
    }

    async fn cache_paths(&self, input: &ConfigInput) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let storage = self.state.app_config.config.load().storage_dir.clone();
        for source in &input.epg.as_ref().unwrap().sources {
            paths.push(tuliprox_processing::epg::get_input_raw_epg_file_path(source, input, &storage).await.unwrap());
        }
        paths
    }
}

async fn root_rotation_refreshes_auto_epg(persistence: Persistence) {
    for hot_reload in [false, true] {
        eprintln!("{persistence:?} core download, hot_reload={hot_reload}");
        let fixture = Fixture::new(persistence, hot_reload, SourceSet::Auto).await;
        let ctx = fixture.context();
        let old = fixture.input();
        let boundary = fixture.server.rotation_boundary();
        fixture.rotate("new-user", "new-pass").await;
        assert!(fixture.server.requests.lock().unwrap().is_empty());
        let current = fixture.input();
        assert_eq!(current.username.as_deref(), Some("new-user"));
        assert_eq!(
            current.epg.as_ref().unwrap().sources[0].url,
            format!("{}/xmltv.php?username=new-user&password=new-pass", fixture.server.base)
        );
        assert_eq!(old.username.as_deref(), Some("old-user"));
        assert!(old.epg.as_ref().unwrap().sources[0].url.contains("username=old-user"));
        let contents = fixture.download(&ctx, &current).await;
        assert!(contents[0].contains("<display-name>new-user</display-name>"));
        assert!(!contents[0].contains("old-user"));
        fixture.server.assert_requests_since(boundary, &["/xmltv.php?username=new-user&password=new-pass"]);
        fixture.assert_persistence().await;
    }
}

#[tokio::test]
async fn yaml_root_rotation_refreshes_auto_epg_before_download() {
    root_rotation_refreshes_auto_epg(Persistence::Yaml).await;
}

#[tokio::test]
async fn csv_root_rotation_refreshes_auto_epg_before_download() {
    root_rotation_refreshes_auto_epg(Persistence::Csv).await;
}

#[tokio::test]
async fn root_rotation_preserves_explicit_epg_urls() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        for hot_reload in [false, true] {
            for source_set in [SourceSet::Mixed, SourceSet::Explicit] {
                let fixture = Fixture::new(persistence, hot_reload, source_set).await;
                let old = fixture.input();
                let smart_match = format!("{:?}", old.epg.as_ref().unwrap().smart_match);
                fixture.rotate("new-user", "new-pass").await;
                fixture.assert_explicit_sources();
                assert_eq!(format!("{:?}", fixture.input().epg.as_ref().unwrap().smart_match), smart_match);
                if matches!(source_set, SourceSet::Explicit) {
                    let boundary = fixture.server.rotation_boundary();
                    let contents = fixture.download(&fixture.context(), &fixture.input()).await;
                    assert!(contents[0].contains("<display-name>old-user</display-name>"));
                    fixture.server.assert_requests_since(boundary, &["/xmltv.php?username=old-user&password=old-pass"]);
                }
                fixture.assert_persistence().await;
            }
        }
    }
}

#[tokio::test]
async fn root_rotation_preserves_explicit_url_equal_to_previous_auto_url() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        for hot_reload in [false, true] {
            let fixture = Fixture::new(persistence, hot_reload, SourceSet::Collision).await;
            let old = fixture.input();
            assert_eq!(old.epg.as_ref().unwrap().sources.len(), 1);
            let ctx = fixture.context();
            let boundary = fixture.server.rotation_boundary();
            fixture.rotate("new-user", "new-pass").await;
            let current = fixture.input();
            assert_eq!(current.epg.as_ref().unwrap().sources.len(), 2);
            assert!(current.epg.as_ref().unwrap().sources[0].url.contains("username=new-user"));
            fixture.assert_explicit_sources();
            let contents = fixture.download(&ctx, &current).await;
            assert!(contents[0].contains("<display-name>new-user</display-name>"));
            assert!(contents[1].contains("<display-name>old-user</display-name>"));
            fixture.server.assert_requests_since(
                boundary,
                &["/xmltv.php?username=new-user&password=new-pass", "/xmltv.php?username=old-user&password=old-pass"],
            );
            assert_eq!(old.epg.as_ref().unwrap().sources.len(), 1);
            fixture.assert_persistence().await;
        }
    }
}

#[tokio::test]
async fn repeated_root_rotations_keep_auto_source_origin() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        let fixture = Fixture::new(persistence, false, SourceSet::Mixed).await;
        for username in ["new-user", "latest-user"] {
            fixture.rotate(username, "updated-pass").await;
            let input = fixture.input();
            assert_eq!(
                input.epg.as_ref().unwrap().sources[0].url,
                format!("{}/xmltv.php?username={username}&password=updated-pass", fixture.server.base)
            );
            fixture.assert_explicit_sources();
            fixture.assert_persistence().await;
        }
    }
}

#[tokio::test]
async fn unchanged_root_credentials_keep_epg_sources_stable() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        let fixture = Fixture::new(persistence, false, SourceSet::Mixed).await;
        let old = fixture.input();
        let old_paths = fixture.cache_paths(&old).await;
        fixture.rotate("old-user", "old-pass").await;
        assert_eq!(format!("{:?}", fixture.input().epg), format!("{:?}", old.epg));
        assert_eq!(fixture.cache_paths(&fixture.input()).await, old_paths);
        fixture.rotate("new-user", "new-pass").await;
        let rotated = fixture.input();
        let rotated_paths = fixture.cache_paths(&rotated).await;
        fixture.rotate("new-user", "new-pass").await;
        assert_eq!(format!("{:?}", fixture.input().epg), format!("{:?}", rotated.epg));
        assert_eq!(fixture.cache_paths(&fixture.input()).await, rotated_paths);
    }
}

#[tokio::test]
async fn root_rotation_keeps_inputs_without_epg_unchanged() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        let fixture = Fixture::new(persistence, false, SourceSet::Missing).await;
        fixture.rotate("new-user", "new-pass").await;
        assert!(fixture.input().epg.is_none());
        assert_eq!(fixture.download(&fixture.context(), &fixture.input()).await, Vec::<String>::new());
        fixture.server.assert_requests_since(0, &[]);
        fixture.assert_persistence().await;
    }
}

#[tokio::test]
async fn root_rotation_isolated_between_inputs_of_same_provider() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        let fixture = Fixture::new(persistence, false, SourceSet::Auto).await;
        let mut peer = ConfigInputDto {
            id: 20,
            name: "peer-input".into(),
            input_type: InputType::Xtream,
            url: fixture.server.base.clone(),
            username: Some("peer-user".into()),
            password: Some("peer-pass".into()),
            epg: Some(EpgConfigDto {
                sources: Some(vec![EpgSourceDto { url: "auto".into(), ..Default::default() }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        if matches!(persistence, Persistence::Yaml) {
            let mut doc: SourcesConfigDto = serde_saphyr::from_str(&fixture.original).unwrap();
            doc.inputs.push(peer.clone());
            tokio::fs::write(&fixture.path, serde_saphyr::to_string(&doc).unwrap()).await.unwrap();
        }
        peer.prepare_epg(true).unwrap();
        let mut peer = ConfigInput::from(&peer);
        peer.prepare(&[]).unwrap();
        let peer = Arc::new(peer);
        let mut sources = (*fixture.state.app_config.sources.load_full()).clone();
        sources.inputs.push(Arc::clone(&peer));
        fixture.state.app_config.sources.store(Arc::new(sources));
        let before = format!("{peer:?}");
        let ctx = fixture.context();
        let boundary = fixture.server.rotation_boundary();
        fixture.rotate("new-user", "new-pass").await;
        let after = fixture.state.app_config.sources.load_full();
        assert_eq!(format!("{:?}", after.inputs[1]), before);
        assert_eq!(format!("{peer:?}"), before);
        assert!(after.inputs[0].epg.as_ref().unwrap().sources[0].url.contains("username=new-user"));
        let a_contents = fixture.download(&ctx, &after.inputs[0]).await;
        let b_contents = fixture.download(&ctx, &after.inputs[1]).await;
        assert!(a_contents[0].contains("<display-name>new-user</display-name>"));
        assert!(b_contents[0].contains("<display-name>peer-user</display-name>"));
        fixture.server.assert_requests_since(
            boundary,
            &["/xmltv.php?username=new-user&password=new-pass", "/xmltv.php?username=peer-user&password=peer-pass"],
        );
    }
}

#[tokio::test]
async fn root_rotation_changes_auto_cache_path_and_preserves_explicit_cache_paths() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        for hot_reload in [false, true] {
            let fixture = Fixture::new(persistence, hot_reload, SourceSet::Mixed).await;
            let old = fixture.input();
            let old_paths = fixture.cache_paths(&old).await;
            for path in &old_paths {
                tokio::fs::write(path, "<?xml version=\"1.0\"?><tv><channel id=\"old-user\"><display-name>old-user</display-name></channel></tv>").await.unwrap();
            }
            fixture.rotate("new-user", "new-pass").await;
            let current = fixture.input();
            let new_paths = fixture.cache_paths(&current).await;
            assert_ne!(new_paths[0], old_paths[0]);
            assert!(!new_paths[0].exists());
            assert!(old_paths[0].exists());
            for expected in &fixture.explicit_sources {
                let source =
                    current.epg.as_ref().unwrap().sources.iter().position(|source| source.url == expected.url).unwrap();
                let previous =
                    old.epg.as_ref().unwrap().sources.iter().position(|source| source.url == expected.url).unwrap();
                assert_eq!(new_paths[source], old_paths[previous]);
                assert!(new_paths[source].exists());
            }
        }
    }
}

#[tokio::test]
async fn root_rotation_bypasses_previous_accounts_warm_epg_cache() {
    for persistence in [Persistence::Yaml, Persistence::Csv] {
        for hot_reload in [false, true] {
            let fixture = Fixture::new(persistence, hot_reload, SourceSet::Collision).await;
            let ctx = fixture.context();
            let old = fixture.input();
            let old_path = fixture.cache_paths(&old).await.remove(0);
            let initial = fixture.download(&ctx, &old).await;
            assert!(initial[0].contains("<display-name>old-user</display-name>"));
            fixture.server.assert_requests_since(0, &["/xmltv.php?username=old-user&password=old-pass"]);
            assert!(old.cache_duration_seconds > 0);
            let old_modified = tokio::fs::metadata(&old_path).await.unwrap().modified().unwrap();
            assert!(old_modified.elapsed().unwrap().as_secs() < old.cache_duration_seconds);
            let mut foreign = (*old).clone();
            foreign.name = "unrelated-input".into();
            let foreign_path = fixture.cache_paths(&foreign).await.remove(0);
            tokio::fs::write(&foreign_path, "unrelated cache content").await.unwrap();

            let boundary = fixture.server.rotation_boundary();
            fixture.rotate("new-user", "new-pass").await;
            let current = fixture.input();
            let new_paths = fixture.cache_paths(&current).await;
            assert_ne!(new_paths[0], old_path);
            assert_eq!(new_paths[1], old_path);
            let contents = fixture.download(&ctx, &current).await;
            assert_ne!(contents[0], initial[0]);
            assert!(contents[0].contains("<display-name>new-user</display-name>"));
            assert!(!contents[0].contains("old-user"));
            assert_eq!(contents[1], initial[0]);
            fixture.server.assert_requests_since(boundary, &["/xmltv.php?username=new-user&password=new-pass"]);
            assert_eq!(tokio::fs::metadata(&old_path).await.unwrap().modified().unwrap(), old_modified);
            assert_eq!(tokio::fs::read_to_string(&foreign_path).await.unwrap(), "unrelated cache content");
            assert_eq!(fixture.download(&ctx, &current).await, contents);
            fixture.server.assert_requests_since(boundary, &["/xmltv.php?username=new-user&password=new-pass"]);
            eprintln!("{persistence:?}, hot_reload={hot_reload}: warm old cache {old_path:?}, new cache {:?}; preserved explicit cache and fetched new-user", new_paths[0]);
            fixture.assert_explicit_sources();
            fixture.assert_persistence().await;
        }
    }
}

#[tokio::test]
async fn runtime_merge_refreshes_on_each_changed_root_field_and_keeps_old_snapshot() {
    let fixture = Fixture::new(Persistence::Yaml, false, SourceSet::Auto).await;
    let original = fixture.state.app_config.sources.load_full();
    let snapshot = format!("{original:?}");
    for (url, username, password) in [
        (format!("{}/updated", fixture.server.base), "old-user", "old-pass"),
        (fixture.server.base.clone(), "new-user", "old-pass"),
        (fixture.server.base.clone(), "old-user", "new-pass"),
    ] {
        let mut doc = runtime_patch_document(&original);
        let updated = &mut doc.inputs[0];
        updated.url = url;
        updated.username = Some(username.into());
        updated.password = Some(password.into());
        let expected = format!(
            "{}/xmltv.php?username={}&password={}",
            updated.url,
            updated.username.as_deref().unwrap(),
            updated.password.as_deref().unwrap()
        );
        let next = merge_runtime_document(&original, doc).unwrap();
        assert_eq!(next.inputs[0].epg.as_ref().unwrap().sources[0].url, expected);
        assert_eq!(format!("{original:?}"), snapshot);
    }
    let mut invalid = runtime_patch_document(&original);
    invalid.inputs[0].username = None;
    invalid.inputs[0].password = None;
    assert!(merge_runtime_document(&original, invalid).is_err());
    assert_eq!(format!("{original:?}"), snapshot);
    assert!(Arc::ptr_eq(&original, &fixture.state.app_config.sources.load_full()));
}
