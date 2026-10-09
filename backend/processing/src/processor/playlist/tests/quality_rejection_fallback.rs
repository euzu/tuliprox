use super::*;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{
    provider_saturation::build_group_lookup, xtream_const::XTREAM_CLUSTER, ConfigInputOptionsDto,
    ConfigInputUpdateQualityDto, ConfigPaths, EventMessage, EventSink, PlaylistUpdateState, ProcessingOrder,
    SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties, UpdateQualityPolicy,
};
use std::{
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex as StdMutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tuliprox_core::{
    model::{
        ConfigSource, MediaToolCapabilities, SourcesConfig, StagedFilter, TargetExecutionPlan, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    utils::FileLockManager,
};
use tuliprox_repository::{
    count_input_xtream_cluster, get_input_storage_path, get_live_cat_collection_path, get_series_cat_collection_path,
    get_vod_cat_collection_path, load_xtream_target_storage, persist_playlist, xtream_get_item_for_stream_id,
    xtream_get_storage_path, TargetPlaylistPersistOptions,
};

struct TestXtreamServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<StdMutex<Vec<String>>>,
    errors: Arc<StdMutex<Vec<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl TestXtreamServer {
    fn start(candidate_counts: [usize; 3]) -> Self { Self::start_with_responses(fixture_responses(candidate_counts)) }

    fn start_with_responses(responses: HashMap<String, String>) -> Self {
        Self::start_with_statuses(responses, HashMap::new())
    }

    fn start_with_statuses(responses: HashMap<String, String>, statuses: HashMap<String, reqwest::StatusCode>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Xtream fixture server");
        listener.set_nonblocking(true).expect("configure Xtream fixture listener");
        let address = listener.local_addr().expect("Xtream fixture address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let errors = Arc::new(StdMutex::new(Vec::new()));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&requests);
        let worker_errors = Arc::clone(&errors);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => match serve_fixture_request(stream, &responses, &statuses) {
                        Ok(action) => worker_requests.lock().expect("request log lock").push(action),
                        Err(err) => worker_errors.lock().expect("server error lock").push(err.to_string()),
                    },
                    Err(err) if err.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                    Err(err) => {
                        worker_errors.lock().expect("server error lock").push(err.to_string());
                        break;
                    }
                }
            }
        });

        Self { base_url: format!("http://{address}"), stop, requests, errors, worker: Some(worker) }
    }

    fn finish(mut self) -> Vec<String> {
        self.stop_worker();
        let errors = self.errors.lock().expect("server error lock").clone();
        assert!(errors.is_empty(), "Xtream fixture server errors: {errors:?}");
        self.requests.lock().expect("request log lock").clone()
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("Xtream fixture server should stop cleanly");
        }
    }
}

impl Drop for TestXtreamServer {
    fn drop(&mut self) { self.stop_worker(); }
}

#[derive(Clone, Default)]
struct CollectSink(Arc<StdMutex<Vec<EventMessage>>>);

impl EventSink for CollectSink {
    fn emit(&self, event: EventMessage) { self.0.lock().expect("event sink lock").push(event); }
}

fn fixture_responses(candidate_counts: [usize; 3]) -> HashMap<String, String> {
    let mut responses =
        HashMap::from([("login".to_string(), serde_json::json!({"user_info": {"status": "Active"}}).to_string())]);
    for (cluster, count) in XTREAM_CLUSTER.into_iter().zip(candidate_counts) {
        let (category_action, stream_action, category_id, id_field, id_base) = match cluster {
            XtreamCluster::Live => ("get_live_categories", "get_live_streams", 1_u32, "stream_id", 1_000_u32),
            XtreamCluster::Video => ("get_vod_categories", "get_vod_streams", 2, "stream_id", 2_000),
            XtreamCluster::Series => ("get_series_categories", "get_series", 3, "series_id", 3_000),
        };
        responses.insert(
            category_action.to_string(),
            serde_json::json!([{"category_id": category_id, "category_name": format!("candidate-{cluster}")}])
                .to_string(),
        );
        let streams: Vec<_> = (0..count)
            .map(|offset| {
                serde_json::json!({
                    "name": format!("candidate-{cluster}-{offset}"),
                    id_field: id_base + u32::try_from(offset).expect("fixture count fits u32"),
                    "category_id": category_id,
                })
            })
            .collect();
        responses.insert(stream_action.to_string(), serde_json::Value::Array(streams).to_string());
    }
    responses
}

fn live_fixture_responses(categories: &serde_json::Value, streams: Vec<serde_json::Value>) -> HashMap<String, String> {
    HashMap::from([
        ("login".to_string(), serde_json::json!({"user_info": {"status": "Active"}}).to_string()),
        ("get_live_categories".to_string(), categories.to_string()),
        ("get_live_streams".to_string(), serde_json::Value::Array(streams).to_string()),
    ])
}

fn live_fixture_stream(provider_id: u32, category_id: u32, name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "stream_id": provider_id,
        "category_id": category_id,
    })
}

fn serve_fixture_request(
    mut stream: TcpStream,
    responses: &HashMap<String, String>,
    statuses: &HashMap<String, reqwest::StatusCode>,
) -> std::io::Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut request = Vec::with_capacity(1_024);
    let mut buffer = [0_u8; 1_024];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request);
    let action = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|target| target.split_once("action=").map(|(_, value)| value))
        .map_or_else(|| "login".to_string(), |value| value.split('&').next().unwrap_or(value).to_string());
    let body = responses
        .get(&action)
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, format!("unexpected action {action}")))?;
    let status = statuses.get(&action).copied().unwrap_or(reqwest::StatusCode::OK);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(action)
}

fn processing_context(storage_dir: &Path) -> PlaylistProcessingContext<shared::model::NoopSink> {
    processing_context_with_events(storage_dir, shared::model::NoopSink)
}

fn processing_context_with_events<E: EventSink>(storage_dir: &Path, events: E) -> PlaylistProcessingContext<E> {
    let paths = ConfigPaths {
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
    };
    let config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config {
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            disk_based_processing: false,
            ..Config::default()
        })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths)),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    };
    PlaylistProcessingContext {
        client: reqwest::Client::new(),
        run_id: "xtream-quality-test-run".into(),
        execution_order: PlaylistUpdateRunOrder::from(1),
        config: Arc::new(config),
        user_targets: Arc::new(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        events,
        playlist_state: None,
        disabled_headers: None,
        processed_inputs: Arc::new(Mutex::new(HashSet::new())),
        input_completions: Arc::new(Mutex::new(HashMap::new())),
        input_locks: Arc::new(Mutex::new(HashMap::new())),
        provider_manager: None,
        metadata_manager: None,
        pre_processed_inputs: None,
        input_refresh: None,
        library_update_mode: LibraryUpdateMode::ExistingCatalog,
        stalker_refresh_mode: StalkerRefreshMode::Complete,
        partial_refresh: Arc::new(AtomicBool::new(false)),
        had_quality_rejections: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn playlist_update_run_constructor_preserves_queued_identity_and_generates_unique_automatic_ids() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ctx = processing_context(temp.path());
    let queued_id = PlaylistUpdateRunId::from("queued-run");
    let queued = ProcessingRun::for_run(
        queued_id.clone(),
        ctx.client.clone(),
        Arc::clone(&ctx.config),
        Arc::clone(&ctx.user_targets),
        shared::model::NoopSink,
    );
    let automatic_a = ProcessingRun::new(
        ctx.client.clone(),
        Arc::clone(&ctx.config),
        Arc::clone(&ctx.user_targets),
        shared::model::NoopSink,
    );
    let automatic_b = ProcessingRun::new(ctx.client, ctx.config, ctx.user_targets, shared::model::NoopSink);

    assert_eq!(queued.run_id, queued_id);
    assert_ne!(automatic_a.run_id, automatic_b.run_id);
}

#[tokio::test]
async fn playlist_update_run_manual_and_automatic_execution_order_is_generated_and_preserved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let events = CollectSink::default();
    let ctx = processing_context_with_events(temp.path(), events.clone());
    let manual_run_id = PlaylistUpdateRunId::from("manual-run");

    exec_processing(ProcessingRun::for_run(
        manual_run_id.clone(),
        ctx.client.clone(),
        Arc::clone(&ctx.config),
        Arc::clone(&ctx.user_targets),
        events.clone(),
    ))
    .await;
    let automatic_run = ProcessingRun::new(ctx.client, ctx.config, ctx.user_targets, events.clone());
    let automatic_run_id = automatic_run.run_id.clone();
    exec_processing(automatic_run).await;

    let emitted = events.0.lock().expect("event sink lock");
    let terminal_for = |run_id: &PlaylistUpdateRunId| {
        emitted.iter().find_map(|event| match event {
            EventMessage::PlaylistUpdate(summary) if summary.run_id.as_ref() == Some(run_id) => Some(summary),
            _ => None,
        })
    };
    let manual_terminal = terminal_for(&manual_run_id).expect("manual terminal event");
    let automatic_terminal = terminal_for(&automatic_run_id).expect("automatic terminal event");
    let manual_order = manual_terminal.execution_order.expect("manual execution order");
    let automatic_order = automatic_terminal.execution_order.expect("automatic execution order");
    assert!(automatic_order > manual_order);

    let progress_for = |run_id: &PlaylistUpdateRunId| {
        emitted
            .iter()
            .filter_map(|event| match event {
                EventMessage::PlaylistUpdateProgress(progress) if progress.run_id.as_ref() == Some(run_id) => {
                    Some(progress)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let manual_progress = progress_for(&manual_run_id);
    let automatic_progress = progress_for(&automatic_run_id);
    assert!(!manual_progress.is_empty());
    assert!(!automatic_progress.is_empty());
    assert!(manual_progress.iter().all(|progress| progress.execution_order == Some(manual_order)));
    assert!(automatic_progress.iter().all(|progress| progress.execution_order == Some(automatic_order)));
}

fn test_input(
    base_url: &str,
    update_quality: ConfigInputUpdateQualityDto,
    cache_duration_seconds: u64,
    skipped_clusters: &[XtreamCluster],
) -> Arc<ConfigInput> {
    let options = ConfigInputOptionsDto {
        skip_live: skipped_clusters.contains(&XtreamCluster::Live),
        skip_vod: skipped_clusters.contains(&XtreamCluster::Video),
        skip_series: skipped_clusters.contains(&XtreamCluster::Series),
        update_quality,
        ..ConfigInputOptionsDto::default()
    };
    Arc::new(ConfigInput {
        id: 1,
        name: "quality-provider".intern(),
        input_type: InputType::Xtream,
        url: base_url.to_string(),
        username: Some("user".to_string()),
        password: Some("password".to_string()),
        enabled: true,
        options: Some(ConfigInputOptions::from(&options)),
        cache_duration_seconds,
        ..ConfigInput::default()
    })
}

fn baseline_group(cluster: XtreamCluster, category_id: u32, title: &str, first_item_id: u32) -> PlaylistGroup {
    baseline_group_with_count(cluster, category_id, title, first_item_id, 2)
}

fn baseline_group_with_count(
    cluster: XtreamCluster,
    category_id: u32,
    title: &str,
    first_item_id: u32,
    count: usize,
) -> PlaylistGroup {
    let title = title.intern();
    let item_type = PlaylistItemType::from(cluster);
    let channels = (0..count)
        .map(|offset| {
            let offset = u32::try_from(offset).expect("baseline fixture count fits u32");
            let item_id = (first_item_id + offset).to_string().intern();
            let mut header = PlaylistItemHeader {
                id: Arc::clone(&item_id),
                input_stream_id: item_id,
                name: format!("{title}-{offset}").intern(),
                title: format!("{title}-{offset}").intern(),
                group: Arc::clone(&title),
                url: format!("http://old.example/{cluster}/{first_item_id}/{offset}").intern(),
                input_name: "quality-provider".intern(),
                item_type,
                xtream_cluster: cluster,
                category_id,
                ..PlaylistItemHeader::default()
            };
            header.gen_uuid();
            PlaylistItem { header }
        })
        .collect();
    PlaylistGroup { id: category_id, title, channels, xtream_cluster: cluster }
}

async fn seed_live_baseline<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput, first_item_id: u32) {
    let baseline = baseline_group_with_count(XtreamCluster::Live, 1, "old-live", first_item_id, 100);
    let (persisted, error) = tuliprox_repository::persist_input_playlist(&ctx.config, input, vec![baseline]).await;
    assert!(error.is_none(), "Live baseline persistence failed: {error:?}");
    assert_eq!(persisted.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
}

fn baseline_groups() -> Vec<PlaylistGroup> {
    vec![
        baseline_group(XtreamCluster::Live, 1, "old-live", 100),
        baseline_group(XtreamCluster::Video, 2, "old-vod", 200),
        baseline_group(XtreamCluster::Series, 3, "old-series", 300),
    ]
}

async fn seed_baseline<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput) {
    let (persisted, error) = tuliprox_repository::persist_input_playlist(&ctx.config, input, baseline_groups()).await;
    assert!(error.is_none(), "baseline persistence failed: {error:?}");
    assert_baseline(&persisted);
}

fn assert_baseline(groups: &[PlaylistGroup]) {
    for (cluster, category_id, title, item_ids) in [
        (XtreamCluster::Live, 1, "old-live", ["100", "101"]),
        (XtreamCluster::Video, 2, "old-vod", ["200", "201"]),
        (XtreamCluster::Series, 3, "old-series", ["300", "301"]),
    ] {
        let group = groups
            .iter()
            .find(|group| group.xtream_cluster == cluster && group.id == category_id)
            .unwrap_or_else(|| panic!("missing persisted {cluster} group"));
        assert_eq!(group.title.as_ref(), title);
        assert_eq!(group.channels.len(), 2);
        assert!(group.channels.iter().all(|item| item.header.xtream_cluster == cluster));
        assert_eq!(group.channels[0].header.id.as_ref(), item_ids[0]);
        assert_eq!(group.channels[1].header.id.as_ref(), item_ids[1]);
    }
}

async fn input_storage_path<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput) -> PathBuf {
    let storage_dir = ctx.config.config.load().storage_dir.clone();
    get_input_storage_path(&input.name, &storage_dir).await.expect("input storage path")
}

async fn seed_valid_cluster_cache<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput) -> u64 {
    let storage_path = input_storage_path(ctx, input).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("current time").as_secs();
    let mut status = input_cache::InputStatus::default();
    for cluster in XTREAM_CLUSTER {
        status.clusters.insert(
            cluster.as_ref().to_string(),
            input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now, last_update: None },
        );
    }
    input_cache::save_input_status(&storage_path, &status);
    now
}

fn set_refresh_policy<E: EventSink>(
    ctx: &mut PlaylistProcessingContext<E>,
    input: &ConfigInput,
    policy: InputRefreshPolicy,
) {
    ctx.input_refresh = Some(InputRefreshOverride { input_id: input.id, policy });
}

fn xtream_target(use_memory_cache: bool) -> Arc<ConfigTarget> {
    Arc::new(ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: "quality-target".to_string(),
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
        use_memory_cache,
    })
}

fn install_source<E: EventSink>(
    ctx: &PlaylistProcessingContext<E>,
    input: &Arc<ConfigInput>,
    target: &Arc<ConfigTarget>,
) {
    let inputs = vec![Arc::clone(input)];
    ctx.config.sources.store(Arc::new(SourcesConfig {
        batch_files: Vec::new(),
        templates: None,
        provider: Vec::new(),
        group_lookup: build_group_lookup(&inputs),
        inputs,
        sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::clone(target)] }],
    }));
}

async fn seed_xtream_target<E: EventSink>(
    ctx: &PlaylistProcessingContext<E>,
    target: &ConfigTarget,
    playlist_state: &Arc<PlaylistStorageState>,
) -> u32 {
    let mut groups = baseline_groups();
    persist_playlist(
        &ctx.config,
        &mut groups,
        None,
        target,
        Some(playlist_state),
        TargetPlaylistPersistOptions::default(),
    )
    .await
    .expect("target baseline should persist");
    groups
        .iter()
        .find(|group| group.xtream_cluster == XtreamCluster::Video)
        .and_then(|group| group.channels.first())
        .map(|item| item.header.virtual_id.get())
        .expect("target baseline VOD id")
}

async fn assert_target_counts(app_config: &Arc<AppConfig>, target: &ConfigTarget, expected: [usize; 3]) {
    let storage = load_xtream_target_storage(app_config, target).await.expect("target storage should load");
    assert_eq!([storage.live.len(), storage.vod.len(), storage.series.len()], expected);
}

async fn assert_empty_target_categories(
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    clusters: &[XtreamCluster],
) {
    let storage_path = {
        let config = app_config.config.load();
        xtream_get_storage_path(&config, &target.name).expect("Xtream target storage path")
    };
    for cluster in clusters {
        let path = match cluster {
            XtreamCluster::Live => get_live_cat_collection_path(&storage_path),
            XtreamCluster::Video => get_vod_cat_collection_path(&storage_path),
            XtreamCluster::Series => get_series_cat_collection_path(&storage_path),
        };
        let categories: Vec<serde_json::Value> = serde_json::from_slice(
            &tokio::fs::read(&path).await.unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        assert!(categories.is_empty(), "{} should contain no categories", path.display());
    }
}

#[test]
fn refresh_policy_override_is_scoped_to_the_selected_input_id() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let mut ctx = processing_context(temp.path());
    let selected = ConfigInput { id: 17, ..ConfigInput::default() };
    set_refresh_policy(&mut ctx, &selected, InputRefreshPolicy::FORCE);

    assert_eq!(ctx.refresh_policy(17), InputRefreshPolicy::FORCE);
    assert_eq!(ctx.refresh_policy(18), InputRefreshPolicy::NORMAL);
}

fn assert_requested_actions(mut actual: Vec<String>, expected: &[&str]) {
    actual.sort();
    let mut expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
    expected.sort();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn mixed_in_memory_update_keeps_rejected_vod_ready_and_marks_the_run_partial() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 1, 2]);
    let events = CollectSink::default();
    let ctx = processing_context_with_events(temp.path(), events.clone());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
    seed_baseline(&ctx, &input).await;

    let mut result = process_input_job_inner(0, &ctx, &input).await;
    let requests = server.finish();

    assert_eq!(result.state, InputJobState::Ready);
    assert!(result.errors.is_empty(), "quality rejection must not become an error: {:?}", result.errors);
    assert!(ctx.had_quality_rejections.load(Ordering::Acquire));
    assert_eq!(
        PlaylistRunSignals { has_quality_rejections: true, ..PlaylistRunSignals::default() }.state(),
        PlaylistUpdateState::Partial
    );

    let groups = result.source.take().expect("ready input source").take_groups();
    let live = groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Live).expect("accepted Live group");
    let vod = groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Video).expect("retained VOD group");
    let series =
        groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Series).expect("accepted Series group");
    assert_eq!(live.title.as_ref(), "candidate-live");
    assert_eq!(live.channels.len(), 2);
    assert_eq!(vod.title.as_ref(), "old-vod");
    assert_eq!(vod.channels.len(), 2);
    assert_eq!(series.title.as_ref(), "candidate-series");
    assert_eq!(series.channels.len(), 2);

    let storage_path = input_storage_path(&ctx, &input).await;
    let status = input_cache::load_input_status(&storage_path);
    assert_eq!(
        status.clusters.get(XtreamCluster::Live.as_ref()).map(|entry| &entry.status),
        Some(&input_cache::ClusterState::Ok)
    );
    assert_eq!(
        status.clusters.get(XtreamCluster::Video.as_ref()).map(|entry| &entry.status),
        Some(&input_cache::ClusterState::Failed)
    );
    assert_eq!(
        status.clusters.get(XtreamCluster::Series.as_ref()).map(|entry| &entry.status),
        Some(&input_cache::ClusterState::Ok)
    );

    let emitted = events.0.lock().expect("event sink lock");
    let rejection_events = emitted
        .iter()
        .filter_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) if progress.message.contains("cluster 'vod' rejected") => {
                Some(progress)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rejection_events.len(), 1);
    assert_eq!(rejection_events[0].target, "quality-provider");
    assert_eq!(
        rejection_events[0].message,
        "Input 'quality-provider' cluster 'vod' rejected: current=2 candidate=1 threshold=100 quality=50; retaining previous cluster"
    );
    assert_requested_actions(
        requests,
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn coderabbit_stalker_global_failures_mark_only_requested_non_skipped_clusters() {
    use crate::processor::stalker::{download_stalker_playlist, StalkerCluster};

    #[derive(Clone, Copy, Debug)]
    enum FailurePoint {
        Portal,
        Client,
        Storage,
        Handshake,
    }

    for failure in [FailurePoint::Portal, FailurePoint::Client, FailurePoint::Storage, FailurePoint::Handshake] {
        for requested in [None, Some(vec![StalkerCluster::Live, StalkerCluster::Series])] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = processing_context(temp.path());
            let server = TestXtreamServer::start_with_responses(HashMap::from([("handshake".into(), "{}".into())]));
            let mut input =
                (*test_input(&server.base_url, ConfigInputUpdateQualityDto::default(), 0, &[XtreamCluster::Live]))
                    .clone();
            input.input_type = InputType::Stalker;
            input.stalker = Some(tuliprox_core::model::StalkerInputConfig::default());
            match failure {
                FailurePoint::Portal => input.url.clear(),
                FailurePoint::Client => input.url = "://invalid-url".into(),
                FailurePoint::Storage => {
                    let path = input_storage_path(&ctx, &input).await;
                    tokio::fs::write(path.join("stalker"), b"not a directory").await.unwrap();
                }
                FailurePoint::Handshake => {}
            }
            let fetch = download_stalker_playlist(
                &ctx.config,
                &ctx.client,
                &input,
                requested.as_deref(),
                StalkerRefreshMode::Complete,
                true,
                UpdateQualityPolicy::Enforce,
            )
            .await;
            let expected = if requested.is_some() {
                vec![XtreamCluster::Series]
            } else {
                vec![XtreamCluster::Video, XtreamCluster::Series]
            };
            assert_eq!(fetch.failed_clusters, expected, "{failure:?}");
            assert_eq!(fetch.errors.len(), 1, "{failure:?}");
            assert!(!fetch.is_ok());
            assert!(fetch.quality_acceptances.is_empty() && fetch.quality_rejections.is_empty());
            assert!(fetch.force_updates.is_empty());
            let requests = server.finish();
            match failure {
                FailurePoint::Handshake => assert!(!requests.is_empty()),
                FailurePoint::Portal | FailurePoint::Client | FailurePoint::Storage => assert!(requests.is_empty()),
            }
        }
    }
}

#[tokio::test]
async fn coderabbit_xtream_http_failure_preserves_successful_disk_clusters() {
    for quality in [UpdateQualityPolicy::Enforce, UpdateQualityPolicy::Bypass] {
        let temp = tempfile::tempdir().unwrap();
        let server = TestXtreamServer::start_with_statuses(
            fixture_responses([2, 2, 2]),
            HashMap::from([("get_vod_streams".to_owned(), reqwest::StatusCode::BAD_REQUEST)]),
        );
        let ctx = processing_context(temp.path());
        let mut config = (**ctx.config.config.load()).clone();
        config.disk_based_processing = true;
        ctx.config.config.store(Arc::new(config));
        let input =
            test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
        seed_baseline(&ctx, &input).await;

        let fetch = tuliprox_iptv::xtream::download_xtream_playlist(
            &ctx.config,
            &ctx.client,
            &ctx.events,
            &input,
            None,
            quality,
        )
        .await;
        assert_eq!(fetch.failed_clusters, vec![XtreamCluster::Video]);
        assert!(!fetch.is_ok(), "the failed cluster must remain a technical failure");
        assert!(fetch.persisted);
        assert!(fetch.quality_rejections.is_empty());
        let accepted: Vec<_> = match quality {
            UpdateQualityPolicy::Enforce => fetch.quality_acceptances.iter().map(|value| value.cluster).collect(),
            UpdateQualityPolicy::Bypass => fetch.force_updates.iter().map(|value| value.cluster).collect(),
        };
        assert_eq!(accepted, vec![XtreamCluster::Live, XtreamCluster::Series]);

        let storage = input_storage_path(&ctx, &input).await;
        let groups =
            tuliprox_repository::load_input_xtream_playlist(&ctx.config, &storage, &XTREAM_CLUSTER).await.unwrap();
        for (cluster, title) in [
            (XtreamCluster::Live, "candidate-live"),
            (XtreamCluster::Video, "old-vod"),
            (XtreamCluster::Series, "candidate-series"),
        ] {
            let group = groups.iter().find(|group| group.xtream_cluster == cluster).unwrap();
            assert_eq!(group.title.as_ref(), title);
            assert_eq!(group.channels.len(), 2);
        }
        assert_eq!(server.finish().len(), 7);
    }
}

#[tokio::test]
async fn pipeline_transparency_shows_failure_survives_xtream_fetch_and_reload() {
    for disk in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut responses = fixture_responses([2, 2, 2]);
        responses.insert("get_series".to_owned(), "{".to_owned());
        let server = TestXtreamServer::start_with_responses(responses);
        let mut ctx = processing_context(temp.path());
        let mut config = ctx.config.config.load().as_ref().clone();
        config.disk_based_processing = disk;
        ctx.config.config.store(Arc::new(config));
        let input =
            test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 0 }, 0, &[]);
        seed_baseline(&ctx, &input).await;
        set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);
        let mut result = download_input(&ctx, &input, false).await;
        assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
        assert!(!result.errors.is_empty());
        let telemetry = result.input_telemetry.as_ref().unwrap();
        for cluster in &telemetry.clusters {
            if cluster.cluster == XtreamCluster::Series {
                assert_eq!(cluster.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
                assert_eq!(cluster.decision, None);
                assert_eq!(cluster.quality, None);
            } else {
                assert_eq!(cluster.technical_state, None, "do not attribute Shows failure to a sibling");
                assert_eq!(cluster.decision, Some(PlaylistUpdateClusterDecision::Accepted));
            }
            assert_eq!(cluster.active_count, None, "failed batch does not prove final activation");
        }
        let storage = input_storage_path(&ctx, &input).await;
        let saved = input_cache::load_input_status(&storage);
        let shows = saved.clusters["series"].last_update.unwrap();
        assert_eq!(shows.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
        assert_eq!(shows.quality, None);
        assert_eq!(shows.quality_guard_threshold, Some(0));
        assert_eq!(shows.policy, Some(InputRefreshPolicy::FORCE));
        assert!(server.finish().iter().any(|action| action == "get_series"));
    }
}

#[tokio::test]
async fn pipeline_transparency_normal_policy_reuses_a_valid_cluster_cache() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 2, 2]);
    let mut ctx = processing_context(temp.path());
    let input =
        test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 3_600, &[]);
    seed_baseline(&ctx, &input).await;
    seed_valid_cluster_cache(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::NORMAL);

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty());
    assert!(result.storage_error.is_none());
    assert!(result.quality_rejections.is_empty());
    assert_eq!(result.job_state(), InputJobState::Ready);
    assert_baseline(&result.source.take_groups());
    assert!(requests.is_empty(), "valid cache should avoid provider requests: {requests:?}");
    let telemetry = result.input_telemetry.as_ref().expect("real cache acquisition telemetry");
    assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::NORMAL);
    assert!(telemetry
        .clusters
        .iter()
        .all(|cluster| { cluster.requested && cluster.source == Some(PlaylistUpdateDataSource::Cache) }));
}

async fn assert_provider_acquisition_survives_in_run_reuse(policy: InputRefreshPolicy) {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 2, 2]);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 95, vod: 95, series: 95 }, 0, &[]);
    seed_baseline(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, policy);

    let mut first = download_input(&ctx, &input, false).await;

    assert!(first.errors.is_empty(), "first provider acquisition failed: {:?}", first.errors);
    assert!(first.storage_error.is_none());
    assert_eq!(first.source.get_channel_count(), 6);
    let first_telemetry = first.input_telemetry.as_ref().expect("provider acquisition telemetry");
    assert_eq!(first_telemetry.refresh_policy, policy);
    assert!(first_telemetry
        .clusters
        .iter()
        .all(|cluster| { cluster.requested && cluster.source == Some(PlaylistUpdateDataSource::Provider) }));

    let storage_path = input_storage_path(&ctx, &input).await;
    let status_after_acquisition = input_cache::load_input_status(&storage_path);
    assert!(XTREAM_CLUSTER.iter().all(|cluster| {
        status_after_acquisition.clusters[cluster.as_ref()].last_update.as_ref().is_some_and(|snapshot| {
            snapshot.policy == Some(policy) && snapshot.source == Some(PlaylistUpdateDataSource::Provider)
        })
    }));

    // A second source/target in the same run reuses the persisted input. It is not a
    // second acquisition and therefore must not emit replacement cache telemetry.
    let mut reused = download_input(&ctx, &input, false).await;

    assert!(reused.errors.is_empty(), "in-run reuse failed: {:?}", reused.errors);
    assert!(reused.storage_error.is_none());
    assert_eq!(reused.source.get_channel_count(), 6);
    assert_eq!(reused.input_telemetry, None);
    assert_eq!(input_cache::load_input_status(&storage_path), status_after_acquisition);
    assert_requested_actions(
        server.finish(),
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn in_run_reuse_after_force_keeps_provider_acquisition_and_snapshot() {
    assert_provider_acquisition_survives_in_run_reuse(InputRefreshPolicy::FORCE).await;
}

#[tokio::test]
async fn in_run_reuse_after_refresh_keeps_provider_acquisition_and_snapshot() {
    assert_provider_acquisition_survives_in_run_reuse(InputRefreshPolicy::REFRESH).await;
}

#[tokio::test]
async fn in_run_reuse_after_normal_provider_fetch_keeps_provider_acquisition_and_snapshot() {
    assert_provider_acquisition_survives_in_run_reuse(InputRefreshPolicy::NORMAL).await;
}

#[tokio::test]
async fn in_run_reuse_for_parallel_sources_emits_exactly_one_provider_acquisition() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 2, 2]);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 95, vod: 95, series: 95 }, 0, &[]);
    seed_baseline(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);

    let (mut first_source, mut second_source) =
        tokio::join!(download_input(&ctx, &input, false), download_input(&ctx, &input, false));

    assert!(first_source.errors.is_empty(), "first source failed: {:?}", first_source.errors);
    assert!(second_source.errors.is_empty(), "second source failed: {:?}", second_source.errors);
    assert_eq!(first_source.source.get_channel_count(), 6);
    assert_eq!(second_source.source.get_channel_count(), 6);
    let acquisition_telemetry = [first_source.input_telemetry.as_ref(), second_source.input_telemetry.as_ref()];
    assert_eq!(acquisition_telemetry.iter().filter(|telemetry| telemetry.is_some()).count(), 1);
    let acquisition = acquisition_telemetry.into_iter().flatten().next().expect("one provider acquisition");
    assert_eq!(acquisition.refresh_policy, InputRefreshPolicy::FORCE);
    assert!(acquisition
        .clusters
        .iter()
        .all(|cluster| { cluster.requested && cluster.source == Some(PlaylistUpdateDataSource::Provider) }));
    assert_requested_actions(
        server.finish(),
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn refresh_policy_bypasses_valid_cache_and_keeps_quality_enforced() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 1, 2]);
    let mut ctx = processing_context(temp.path());
    let input =
        test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 3_600, &[]);
    seed_baseline(&ctx, &input).await;
    seed_valid_cluster_cache(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::REFRESH);

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty());
    assert!(result.storage_error.is_none());
    assert_eq!(result.job_state(), InputJobState::Ready);
    assert_eq!(result.quality_rejections.len(), 1);
    assert_eq!(result.quality_rejections[0].cluster, XtreamCluster::Video);
    let groups = result.source.take_groups();
    assert_eq!(
        groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Video).map(|group| group.title.as_ref()),
        Some("old-vod")
    );
    assert_requested_actions(
        requests,
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn force_policy_bypasses_valid_cache_and_quality_and_accepts_an_empty_cluster() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 0, 1]);
    let events = CollectSink::default();
    let mut ctx = processing_context_with_events(temp.path(), events.clone());
    let input =
        test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 95, series: 100 }, 3_600, &[]);
    seed_baseline(&ctx, &input).await;
    seed_valid_cluster_cache(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);

    let mut result = process_input_job_inner(0, &ctx, &input).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "forced update failed: {:?}", result.errors);
    assert_eq!(result.state, InputJobState::Ready);
    let groups = result.source.take().expect("forced input source").take_groups();
    assert_eq!(
        groups
            .iter()
            .filter(|group| group.xtream_cluster == XtreamCluster::Live)
            .map(|group| group.channels.len())
            .sum::<usize>(),
        2
    );
    assert!(groups.iter().all(|group| group.xtream_cluster != XtreamCluster::Video));
    assert_eq!(
        groups
            .iter()
            .filter(|group| group.xtream_cluster == XtreamCluster::Series)
            .map(|group| group.channels.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        PlaylistRunSignals {
            has_error: !result.errors.is_empty(),
            has_pending_stalker_refresh: ctx.partial_refresh.load(Ordering::Acquire),
            has_quality_rejections: ctx.had_quality_rejections.load(Ordering::Acquire),
        }
        .state(),
        PlaylistUpdateState::Success
    );

    let storage_path = input_storage_path(&ctx, &input).await;
    assert_eq!(
        count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Video)
            .await
            .expect("forced empty VOD should remain countable"),
        Some(0)
    );
    let status = input_cache::load_input_status(&storage_path);
    assert!(XTREAM_CLUSTER.iter().all(|cluster| {
        status.clusters.get(cluster.as_ref()).map(|entry| &entry.status) == Some(&input_cache::ClusterState::Ok)
    }));
    let messages = events
        .0
        .lock()
        .expect("event sink lock")
        .iter()
        .filter_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) => Some(progress.message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| {
        message
            == "Input 'quality-provider': forced update requested; cache and update-quality bypassed for live,vod,series"
    }));
    assert!(messages.iter().any(|message| {
        message == "Input 'quality-provider' cluster 'vod' force-published: candidate=0 configured_threshold=95"
    }));
    assert_requested_actions(
        requests,
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn force_empty_vod_is_published_to_xtream_target_and_memory_cache() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([2, 0, 1]);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 95, series: 100 }, 0, &[]);
    let target = xtream_target(true);
    let playlist_state = Arc::new(PlaylistStorageState::new());
    ctx.playlist_state = Some(Arc::clone(&playlist_state));
    seed_baseline(&ctx, &input).await;
    let old_vod_id = seed_xtream_target(&ctx, &target, &playlist_state).await;
    install_source(&ctx, &input, &target);
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);
    let app_config = Arc::clone(&ctx.config);
    let pending_refresh = Arc::clone(&ctx.partial_refresh);
    let quality_rejections = Arc::clone(&ctx.had_quality_rejections);

    let (_, target_stats, errors) = process_source(0, Arc::new(ctx)).await;
    server.finish();

    assert!(errors.is_empty(), "forced target update failed: {errors:?}");
    assert_eq!(target_stats.len(), 1);
    assert!(target_stats[0].success);
    assert_eq!(
        PlaylistRunSignals {
            has_error: false,
            has_pending_stalker_refresh: pending_refresh.load(Ordering::Acquire),
            has_quality_rejections: quality_rejections.load(Ordering::Acquire),
        }
        .state(),
        PlaylistUpdateState::Success
    );
    assert_target_counts(&app_config, &target, [2, 0, 1]).await;
    assert_empty_target_categories(&app_config, &target, &[XtreamCluster::Video]).await;
    assert!(
        xtream_get_item_for_stream_id(old_vod_id, &app_config, &playlist_state, &target, Some(XtreamCluster::Video),)
            .await
            .is_err(),
        "old VOD id must not remain client-visible"
    );
    let cache = playlist_state.data.read().await;
    let cached =
        cache.get(&target.name).and_then(|storage| storage.xtream.as_ref()).expect("Xtream target memory cache");
    assert_eq!([cached.live.len(), cached.vod.len(), cached.series.len()], [2, 0, 1]);
}

#[tokio::test]
async fn force_fully_empty_result_clears_the_xtream_target() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([0, 0, 0]);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 90, vod: 90, series: 90 }, 0, &[]);
    let target = xtream_target(true);
    let playlist_state = Arc::new(PlaylistStorageState::new());
    ctx.playlist_state = Some(Arc::clone(&playlist_state));
    seed_baseline(&ctx, &input).await;
    let old_vod_id = seed_xtream_target(&ctx, &target, &playlist_state).await;
    install_source(&ctx, &input, &target);
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);
    let app_config = Arc::clone(&ctx.config);
    let pending_refresh = Arc::clone(&ctx.partial_refresh);
    let quality_rejections = Arc::clone(&ctx.had_quality_rejections);

    let (_, target_stats, errors) = process_source(0, Arc::new(ctx)).await;
    server.finish();

    assert!(errors.is_empty(), "fully empty forced target update failed: {errors:?}");
    assert_eq!(target_stats.len(), 1);
    assert!(target_stats[0].success);
    assert_eq!(
        PlaylistRunSignals {
            has_error: false,
            has_pending_stalker_refresh: pending_refresh.load(Ordering::Acquire),
            has_quality_rejections: quality_rejections.load(Ordering::Acquire),
        }
        .state(),
        PlaylistUpdateState::Success
    );
    assert_target_counts(&app_config, &target, [0, 0, 0]).await;
    assert_empty_target_categories(&app_config, &target, &XTREAM_CLUSTER).await;
    assert!(
        xtream_get_item_for_stream_id(old_vod_id, &app_config, &playlist_state, &target, Some(XtreamCluster::Video),)
            .await
            .is_err(),
        "fully empty target must not resolve an old VOD id"
    );
    let cache = playlist_state.data.read().await;
    let cached =
        cache.get(&target.name).and_then(|storage| storage.xtream.as_ref()).expect("Xtream target memory cache");
    assert_eq!([cached.live.len(), cached.vod.len(), cached.series.len()], [0, 0, 0]);
}

#[tokio::test]
async fn force_technical_parse_failure_retains_the_target_cluster() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let mut responses = fixture_responses([2, 1, 2]);
    responses.insert("get_vod_streams".to_string(), "{".to_string());
    let server = TestXtreamServer::start_with_responses(responses);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
    let target = xtream_target(true);
    let playlist_state = Arc::new(PlaylistStorageState::new());
    ctx.playlist_state = Some(Arc::clone(&playlist_state));
    seed_baseline(&ctx, &input).await;
    let old_vod_id = seed_xtream_target(&ctx, &target, &playlist_state).await;
    install_source(&ctx, &input, &target);
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);
    let app_config = Arc::clone(&ctx.config);

    let (_, target_stats, errors) = process_source(0, Arc::new(ctx)).await;
    server.finish();

    assert!(!errors.is_empty(), "malformed provider response must remain a technical error");
    assert_eq!(target_stats.len(), 1);
    assert!(target_stats[0].success, "usable fallback should still be published");
    assert_target_counts(&app_config, &target, [2, 2, 2]).await;
    let retained_vod =
        xtream_get_item_for_stream_id(old_vod_id, &app_config, &playlist_state, &target, Some(XtreamCluster::Video))
            .await
            .expect("old VOD should remain client-visible after technical failure");
    assert!(retained_vod.name.starts_with("old-vod"));
}

#[tokio::test]
async fn normal_empty_candidate_retains_the_xtream_target_without_empty_authorization() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([0, 0, 0]);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
    let target = xtream_target(true);
    let playlist_state = Arc::new(PlaylistStorageState::new());
    ctx.playlist_state = Some(Arc::clone(&playlist_state));
    seed_baseline(&ctx, &input).await;
    let old_vod_id = seed_xtream_target(&ctx, &target, &playlist_state).await;
    install_source(&ctx, &input, &target);
    let app_config = Arc::clone(&ctx.config);
    let quality_rejections = Arc::clone(&ctx.had_quality_rejections);

    let (_, target_stats, errors) = process_source(0, Arc::new(ctx)).await;
    server.finish();

    assert!(errors.is_empty(), "quality rejection must not become a technical error: {errors:?}");
    assert!(quality_rejections.load(Ordering::Acquire));
    assert_eq!(target_stats.len(), 1);
    assert!(target_stats[0].success);
    assert_target_counts(&app_config, &target, [2, 2, 2]).await;
    let retained_vod =
        xtream_get_item_for_stream_id(old_vod_id, &app_config, &playlist_state, &target, Some(XtreamCluster::Video))
            .await
            .expect("normal empty candidate must retain the target VOD");
    assert!(retained_vod.name.starts_with("old-vod"));
}

#[tokio::test]
async fn force_policy_retains_a_cluster_after_a_technical_parse_failure() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let mut responses = fixture_responses([2, 1, 2]);
    responses.insert("get_vod_streams".to_string(), "{".to_string());
    let server = TestXtreamServer::start_with_responses(responses);
    let mut ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
    seed_baseline(&ctx, &input).await;
    set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(!result.errors.is_empty(), "malformed VOD response must remain a technical error");
    assert!(result.storage_error.is_none());
    assert!(result.quality_rejections.is_empty());
    let groups = result.source.take_groups();
    let vod = groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Video).expect("retained VOD");
    assert_eq!(vod.title.as_ref(), "old-vod");
    assert_eq!(vod.channels.len(), 2);
    assert_eq!(
        groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Live).map(|group| group.title.as_ref()),
        Some("candidate-live")
    );
    assert_eq!(
        groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Series).map(|group| group.title.as_ref()),
        Some("candidate-series")
    );
    assert_requested_actions(
        requests,
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn in_memory_series_quality_compares_catalog_rows_with_an_enriched_baseline() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([0, 0, 1]);
    let ctx = processing_context(temp.path());
    let input = test_input(
        &server.base_url,
        ConfigInputUpdateQualityDto { series: 100, ..ConfigInputUpdateQualityDto::default() },
        0,
        &[XtreamCluster::Live, XtreamCluster::Video],
    );
    let mut baseline = baseline_group(XtreamCluster::Series, 3, "old-series", 3_000);
    baseline.channels.truncate(1);
    let episodes = (0..200_u32)
        .map(|id| SeriesStreamDetailEpisodeProperties { id, ..SeriesStreamDetailEpisodeProperties::default() })
        .collect();
    baseline.channels[0].header.additional_properties =
        Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
            series_id: 3_000,
            details: Some(SeriesStreamDetailProperties::new(None, Vec::new(), Some(episodes))),
            ..SeriesStreamProperties::default()
        })));
    let (persisted, error) = tuliprox_repository::persist_input_playlist(&ctx.config, &input, vec![baseline]).await;
    assert!(error.is_none(), "enriched baseline persistence failed: {error:?}");
    assert_eq!(persisted.iter().map(|group| group.channels.len()).sum::<usize>(), 1);
    assert_eq!(
        count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Series)
            .await
            .expect("enriched baseline should be countable"),
        Some(1)
    );

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "Series update failed: {:?}", result.errors);
    assert!(result.quality_rejections.is_empty());
    let groups = result.source.take_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "candidate-series");
    assert_eq!(groups[0].channels.len(), 1);
    let episode_count = groups[0].channels[0]
        .header
        .additional_properties
        .as_ref()
        .and_then(|properties| match properties {
            StreamProperties::Series(series) => series.details.as_ref(),
            _ => None,
        })
        .and_then(|details| details.episodes.as_ref())
        .map(Vec::len);
    assert_eq!(episode_count, Some(200));
    assert_requested_actions(requests, &["login", "get_series_categories", "get_series"]);
}

#[tokio::test]
async fn in_memory_quality_rejection_counts_duplicate_provider_ids_once_and_retains_the_baseline() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let first_provider_id = 1_000_u32;
    let streams = (0..50_u32)
        .flat_map(|offset| {
            let provider_id = first_provider_id + offset;
            [
                live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}")),
                live_fixture_stream(provider_id, 1, &format!("duplicate-{provider_id}")),
            ]
        })
        .collect();
    let responses =
        live_fixture_responses(&serde_json::json!([{"category_id": 1, "category_name": "candidate-live"}]), streams);
    let server = TestXtreamServer::start_with_responses(responses);
    let ctx = processing_context(temp.path());
    let input = test_input(
        &server.base_url,
        ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
        0,
        &[XtreamCluster::Video, XtreamCluster::Series],
    );
    seed_live_baseline(&ctx, &input, first_provider_id).await;

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "duplicate rejection must not become an error: {:?}", result.errors);
    assert!(result.storage_error.is_none());
    assert_eq!(
        result.quality_rejections,
        vec![ClusterUpdateRejection {
            cluster: XtreamCluster::Live,
            current_count: 100,
            candidate_count: 50,
            threshold: 100,
            quality: 50,
        }]
    );
    let groups = result.source.take_groups();
    assert_eq!(groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
    assert_eq!(groups[0].title.as_ref(), "old-live");

    let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("reload retained Live baseline");
    let reloaded_groups = reloaded.take_groups();
    assert_eq!(reloaded_groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
    assert_eq!(reloaded_groups[0].title.as_ref(), "old-live");
    assert_eq!(
        count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Live)
            .await
            .expect("retained baseline should be countable"),
        Some(100)
    );
    assert_requested_actions(requests, &["login", "get_live_categories", "get_live_streams"]);
}

#[tokio::test]
async fn in_memory_quality_accepts_unique_population_and_persists_the_last_duplicate_row() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let first_provider_id = 1_000_u32;
    let winning_provider_id = first_provider_id + 42;
    let mut streams = (0..100_u32)
        .map(|offset| {
            let provider_id = first_provider_id + offset;
            live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}"))
        })
        .collect::<Vec<_>>();
    streams.push(live_fixture_stream(winning_provider_id, 2, "winning-duplicate"));
    let responses = live_fixture_responses(
        &serde_json::json!([
            {"category_id": 1, "category_name": "candidate-live"},
            {"category_id": 2, "category_name": "winning-category"}
        ]),
        streams,
    );
    let server = TestXtreamServer::start_with_responses(responses);
    let ctx = processing_context(temp.path());
    let input = test_input(
        &server.base_url,
        ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
        0,
        &[XtreamCluster::Video, XtreamCluster::Series],
    );
    seed_live_baseline(&ctx, &input, first_provider_id).await;

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "deduplicated acceptance failed: {:?}", result.errors);
    assert!(result.storage_error.is_none());
    assert!(result.quality_rejections.is_empty());
    let groups = result.source.take_groups();
    assert_eq!(groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
    let winner_group = groups.iter().find(|group| group.id == 2).expect("winning category");
    assert_eq!(winner_group.title.as_ref(), "winning-category");
    assert_eq!(winner_group.channels.len(), 1);
    assert_eq!(winner_group.channels[0].header.id.as_ref(), winning_provider_id.to_string());
    assert_eq!(winner_group.channels[0].header.name.as_ref(), "winning-duplicate");
    assert_eq!(winner_group.channels[0].header.category_id, 2);

    let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("reload accepted Live candidate");
    let reloaded_groups = reloaded.take_groups();
    assert_eq!(reloaded_groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
    let reloaded_winner = reloaded_groups.iter().find(|group| group.id == 2).expect("persisted winning category");
    assert_eq!(reloaded_winner.channels.len(), 1);
    assert_eq!(reloaded_winner.channels[0].header.id.as_ref(), winning_provider_id.to_string());
    assert_eq!(reloaded_winner.channels[0].header.name.as_ref(), "winning-duplicate");
    assert_eq!(reloaded_winner.channels[0].header.category_id, 2);
    assert_eq!(
        count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Live)
            .await
            .expect("accepted candidate should be countable"),
        Some(100)
    );
    assert_requested_actions(requests, &["login", "get_live_categories", "get_live_streams"]);
}

#[tokio::test]
async fn m3u_update_quality_enforces_the_existing_cluster_threshold() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let playlist_path = temp.path().join("input.m3u");
    tokio::fs::write(
        &playlist_path,
        "#EXTM3U\n#EXTINF:-1,Old One\nhttp://old.example/1\n#EXTINF:-1,Old Two\nhttp://old.example/2\n",
    )
    .await
    .expect("initial M3U fixture");
    let options = ConfigInputOptionsDto {
        update_quality: ConfigInputUpdateQualityDto { live: 100, vod: 0, series: 0 },
        ..ConfigInputOptionsDto::default()
    };
    let input = Arc::new(ConfigInput {
        id: 2,
        name: "m3u-quality-guard".intern(),
        input_type: InputType::M3u,
        url: playlist_path.to_string_lossy().into_owned(),
        enabled: true,
        options: Some(ConfigInputOptions::from(&options)),
        ..ConfigInput::default()
    });
    let initial_ctx = processing_context(temp.path());

    let mut initial = download_input(&initial_ctx, &input, false).await;

    assert!(initial.errors.is_empty(), "initial M3U download failed: {:?}", initial.errors);
    assert!(initial.quality_rejections.is_empty());
    assert_eq!(initial.source.get_channel_count(), 2);

    tokio::fs::write(&playlist_path, "#EXTM3U\n#EXTINF:-1,New\nhttp://new.example/1\n")
        .await
        .expect("replacement M3U fixture");
    let refreshed_ctx = processing_context(temp.path());
    let mut refreshed = download_input(&refreshed_ctx, &input, false).await;

    assert!(refreshed.errors.is_empty(), "replacement M3U download failed: {:?}", refreshed.errors);
    assert!(refreshed.storage_error.is_none());
    assert_eq!(refreshed.quality_rejections.len(), 1);
    assert_eq!(refreshed.quality_rejections[0].cluster, XtreamCluster::Live);
    assert_eq!(refreshed.source.get_channel_count(), 2);
    let mut reloaded =
        load_input_playlist(&refreshed_ctx.config, &input, None).await.expect("replacement M3U should be persisted");
    assert_eq!(reloaded.get_channel_count(), 2);
}

#[tokio::test]
async fn quality_rejection_for_only_requested_vod_loads_complete_persisted_input() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([0, 1, 0]);
    let ctx = processing_context(temp.path());
    let input = test_input(
        &server.base_url,
        ConfigInputUpdateQualityDto { vod: 100, ..ConfigInputUpdateQualityDto::default() },
        3_600,
        &[],
    );
    seed_baseline(&ctx, &input).await;
    let storage_path = input_storage_path(&ctx, &input).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("current time").as_secs();
    let mut status = input_cache::InputStatus::default();
    status.clusters.insert(
        XtreamCluster::Live.as_ref().to_string(),
        input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now, last_update: None },
    );
    status.clusters.insert(
        XtreamCluster::Video.as_ref().to_string(),
        input_cache::ClusterStatus {
            status: input_cache::ClusterState::Ok,
            timestamp: now.saturating_sub(3_601),
            last_update: None,
        },
    );
    status.clusters.insert(
        XtreamCluster::Series.as_ref().to_string(),
        input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now, last_update: None },
    );
    input_cache::save_input_status(&storage_path, &status);

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "quality rejection must not become an error: {:?}", result.errors);
    assert!(result.storage_error.is_none());
    assert!(!result.partial);
    assert_eq!(result.job_state(), InputJobState::Ready);
    assert_eq!(result.quality_rejections.len(), 1);
    assert_eq!(
        result.quality_rejections[0],
        ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 2,
            candidate_count: 1,
            threshold: 100,
            quality: 50,
        }
    );
    assert_eq!(
        PlaylistRunSignals {
            has_quality_rejections: !result.quality_rejections.is_empty(),
            ..PlaylistRunSignals::default()
        }
        .state(),
        PlaylistUpdateState::Partial
    );
    assert_baseline(&result.source.take_groups());

    let status = input_cache::load_input_status(&storage_path);
    let live = status.clusters.get(XtreamCluster::Live.as_ref()).expect("Live cache status");
    let vod = status.clusters.get(XtreamCluster::Video.as_ref()).expect("VOD cache status");
    let series = status.clusters.get(XtreamCluster::Series.as_ref()).expect("Series cache status");
    assert_eq!(live.status, input_cache::ClusterState::Ok);
    assert_eq!(live.timestamp, now);
    assert_eq!(vod.status, input_cache::ClusterState::Failed);
    assert_eq!(series.status, input_cache::ClusterState::Ok);
    assert_eq!(series.timestamp, now);
    assert_requested_actions(requests, &["login", "get_vod_categories", "get_vod_streams"]);
}

#[tokio::test]
async fn all_requested_clusters_rejected_load_the_complete_unchanged_baseline() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([1, 1, 1]);
    let ctx = processing_context(temp.path());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
    seed_baseline(&ctx, &input).await;

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "quality rejections must not become errors: {:?}", result.errors);
    assert!(result.storage_error.is_none());
    assert_eq!(result.job_state(), InputJobState::Ready);
    assert_eq!(result.quality_rejections.iter().map(|rejection| rejection.cluster).collect::<Vec<_>>(), XTREAM_CLUSTER);
    assert!(result.quality_rejections.iter().all(|rejection| {
        rejection.current_count == 2
            && rejection.candidate_count == 1
            && rejection.threshold == 100
            && rejection.quality == 50
    }));
    assert_baseline(&result.source.take_groups());

    let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("reload persisted baseline");
    assert_baseline(&reloaded.take_groups());
    let storage_path = input_storage_path(&ctx, &input).await;
    let status = input_cache::load_input_status(&storage_path);
    assert!(XTREAM_CLUSTER.iter().all(|cluster| {
        status.clusters.get(cluster.as_ref()).map(|entry| &entry.status) == Some(&input_cache::ClusterState::Failed)
    }));
    assert_requested_actions(
        requests,
        &[
            "login",
            "get_live_categories",
            "get_live_streams",
            "get_vod_categories",
            "get_vod_streams",
            "get_series_categories",
            "get_series",
        ],
    );
}

#[tokio::test]
async fn quality_rejection_for_empty_bootstrap_does_not_invent_a_baseline() {
    let temp = tempfile::tempdir().expect("temporary storage");
    let server = TestXtreamServer::start([0, 0, 0]);
    let ctx = processing_context(temp.path());
    let input = test_input(
        &server.base_url,
        ConfigInputUpdateQualityDto { vod: 100, ..ConfigInputUpdateQualityDto::default() },
        0,
        &[XtreamCluster::Live, XtreamCluster::Series],
    );

    let mut result = download_input(&ctx, &input, false).await;
    let requests = server.finish();

    assert!(result.errors.is_empty(), "unexpected bootstrap errors: {:?}", result.errors);
    assert!(result.storage_error.is_none());
    assert_eq!(result.quality_rejections.len(), 1);
    assert_eq!(
        result.quality_rejections[0],
        ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 0,
            candidate_count: 0,
            threshold: 100,
            quality: 0,
        }
    );
    assert!(result.source.is_empty());
    assert_eq!(result.job_state(), InputJobState::Failed);
    let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("empty bootstrap reload");
    assert!(reloaded.take_groups().is_empty());
    assert_requested_actions(requests, &["login", "get_vod_categories", "get_vod_streams"]);
}

mod m3u_update_quality;
mod manual_update_integration;
mod persisted_input_status;
mod staged_completion;
