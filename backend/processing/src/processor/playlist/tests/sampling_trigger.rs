//! The playlist run hands opted-in Stalker inputs to the background sampler.
//!
//! The run itself is covered in `container_extension_sampling`; what matters here is the decision to
//! start it: only an input that asked for it, is a Stalker input, and whose own update published a
//! catalog is queued.

use super::{queue_container_extension_sampling, InputRunOutcome, PlaylistProcessingContext};
use crate::{metadata_sink::MetadataUpdateSink, processor::StalkerRefreshMode};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{
    ConfigInputOptionsDto, ConfigPaths, ContainerExtensionSamplingDto, InputType, NoopSink, PlaylistUpdateRunId,
    PlaylistUpdateRunOrder,
};
use std::{collections::HashMap, sync::Arc};
use tuliprox_core::{
    model::{
        AppConfig, Config, ConfigInput, ConfigInputOptions, MediaToolCapabilities, ProcessTargets, SourcesConfig,
        UpdateTask,
    },
    utils::FileLockManager,
};
use tuliprox_session::ActiveProviderManager;

/// Records the inputs whose sampling the pipeline requested and nothing else.
#[derive(Clone, Default)]
struct RecordingSink {
    queued: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl MetadataUpdateSink for RecordingSink {
    async fn acquire_update_pause_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        Arc::new(tokio::sync::RwLock::new(())).write_owned().await
    }

    async fn prepare_enqueue_state(&self, _input_name: Arc<str>) {}

    fn should_skip_enqueue(&self, _input_name: &str, _task: &UpdateTask) -> bool { true }

    fn queue_task_background(self: Arc<Self>, _input_name: Arc<str>, _task: UpdateTask) {}

    fn queue_container_extension_sampling(self: Arc<Self>, input_name: Arc<str>) {
        self.queued.lock().push(input_name.to_string());
    }
}

fn app_config(inputs: Vec<Arc<ConfigInput>>) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs, ..SourcesConfig::default() })),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        plugins: Arc::default(),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        failover: Arc::default(),
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
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    })
}

fn input(id: u16, name: &str, input_type: InputType, sampling_enabled: bool) -> Arc<ConfigInput> {
    let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
        container_extension_sampling: ContainerExtensionSamplingDto {
            enabled: sampling_enabled,
            ..ContainerExtensionSamplingDto::default()
        },
        ..ConfigInputOptionsDto::default()
    });
    Arc::new(ConfigInput {
        id,
        name: name.into(),
        input_type,
        enabled: true,
        options: Some(options),
        ..ConfigInput::default()
    })
}

fn context(
    app_config: Arc<AppConfig>,
    sink: RecordingSink,
    outcomes: &[(&str, InputRunOutcome)],
) -> PlaylistProcessingContext<NoopSink, RecordingSink> {
    PlaylistProcessingContext {
        client: reqwest::Client::new(),
        run_id: PlaylistUpdateRunId::from("sampling-trigger".to_string()),
        execution_order: PlaylistUpdateRunOrder::from(1),
        config: app_config,
        user_targets: Arc::new(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        events: NoopSink,
        playlist_state: None,
        disabled_headers: None,
        processed_inputs: Arc::new(tokio::sync::Mutex::new(
            outcomes
                .iter()
                .map(|(name, outcome)| (Arc::<str>::from(*name), outcome.clone()))
                .collect::<HashMap<_, _>>(),
        )),
        input_completions: Arc::default(),
        input_locks: Arc::default(),
        epg_downloads: Arc::default(),
        provider_manager: None::<Arc<ActiveProviderManager>>,
        metadata_manager: Some(Arc::new(sink)),
        pre_processed_inputs: None,
        stalker_refresh_mode: StalkerRefreshMode::Complete,
        partial_refresh: Arc::default(),
        had_quality_rejections: Arc::default(),
        replace_target_contents: false,
        publication_deadline: None,
        input_refresh: None,
        library_update_mode: crate::processor::playlist::LibraryUpdateMode::default(),
        maintenance: None,
        override_status: None,
        plugins: shared::model::NoPlugins,
        imdb: crate::imdb::ImdbRatingRuntime::default(),
    }
}

#[tokio::test]
async fn only_a_published_opted_in_stalker_input_is_queued_for_sampling() {
    let sink = RecordingSink::default();
    let queued = Arc::clone(&sink.queued);
    let app_config = app_config(vec![
        input(1, "published", InputType::Stalker, true),
        input(2, "not-opted-in", InputType::Stalker, false),
        input(3, "xtream", InputType::Xtream, true),
        input(4, "failed", InputType::Stalker, true),
        input(5, "not-in-this-run", InputType::Stalker, true),
        input(6, "partial", InputType::Stalker, true),
    ]);

    let ctx = context(
        app_config,
        sink,
        &[
            ("published", InputRunOutcome::Ready),
            ("not-opted-in", InputRunOutcome::Ready),
            ("xtream", InputRunOutcome::Ready),
            ("failed", InputRunOutcome::Failed(None)),
            ("partial", InputRunOutcome::Failed(None)),
        ],
    );

    queue_container_extension_sampling(&ctx).await;

    assert_eq!(queued.lock().as_slice(), ["published"], "a failed, partial, non-Stalker or unopted input is skipped");
}

#[tokio::test]
async fn a_run_without_a_metadata_worker_queues_nothing() {
    let sink = RecordingSink::default();
    let queued = Arc::clone(&sink.queued);
    let mut ctx = context(
        app_config(vec![input(1, "published", InputType::Stalker, true)]),
        sink,
        &[("published", InputRunOutcome::Ready)],
    );
    ctx.metadata_manager = None;

    queue_container_extension_sampling(&ctx).await;

    assert!(queued.lock().is_empty());
}

async fn start_stalker_test_server(
    vod_body: Arc<parking_lot::Mutex<String>>,
    fail_vod: Arc<std::sync::atomic::AtomicBool>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind stalker test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let vod_body = Arc::clone(&vod_body);
            let fail_vod = Arc::clone(&fail_vod);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if req.contains("action=handshake") {
                    ("200 OK", r#"{"js":{"token":"fixture-session"}}"#.to_string())
                } else if req.contains("action=get_profile") || req.contains("action=get_capabilities") {
                    ("200 OK", r#"{"js":{}}"#.to_string())
                } else if req.contains("action=get_genres") {
                    ("200 OK", r#"{"js":[{"id":"1","title":"Live"}]}"#.to_string())
                } else if req.contains("action=get_categories") {
                    ("200 OK", r#"{"js":[{"id":"10","title":"Movies"}]}"#.to_string())
                } else if req.contains("action=get_all_channels") {
                    (
                        "200 OK",
                        r#"{"js":{"data":[{"id":"1","name":"Channel 1","cmd":"ffmpeg http://stream.example/live/1","tv_genre_id":"1"}]}}"#.to_string(),
                    )
                } else if req.contains("type=vod") && req.contains("action=get_ordered_list") {
                    if fail_vod.load(std::sync::atomic::Ordering::Acquire) {
                        ("500 Internal Server Error", r#"{"error":"boom"}"#.to_string())
                    } else {
                        ("200 OK", vod_body.lock().clone())
                    }
                } else if req.contains("action=get_ordered_list") {
                    ("200 OK", r#"{"js":{"data":[],"total_items":0,"max_page_items":10}}"#.to_string())
                } else {
                    ("200 OK", r#"{"js":[]}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}/stalker_portal/c/"), handle)
}

fn stalker_input_with_quality(
    storage_dir: &std::path::Path,
    url: String,
    vod_quality: u8,
) -> (Arc<AppConfig>, Arc<ConfigInput>) {
    use shared::model::ConfigInputUpdateQualityDto;
    use tuliprox_core::model::{StalkerDeviceProfile, StalkerInputConfig};

    let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
        update_quality: ConfigInputUpdateQualityDto { live: 0, vod: vod_quality, series: 0 },
        container_extension_sampling: ContainerExtensionSamplingDto {
            enabled: true,
            ..ContainerExtensionSamplingDto::default()
        },
        ..ConfigInputOptionsDto::default()
    });
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "stalker-portal".into(),
        input_type: InputType::Stalker,
        url,
        enabled: true,
        options: Some(options),
        stalker: Some(StalkerInputConfig {
            device: Some(StalkerDeviceProfile { mac_address: Some("00:1a:79:aa:bb:cc".into()), ..Default::default() }),
            ..Default::default()
        }),
        ..ConfigInput::default()
    });
    let app = app_config(vec![Arc::clone(&input)]);
    app.config.store(Arc::new(Config { storage_dir: storage_dir.display().to_string(), ..Config::default() }));
    (app, input)
}

#[tokio::test]
async fn quality_rejected_or_partially_failed_update_does_not_queue_sampling_when_replace_target_contents_is_false() {
    use crate::processor::playlist::ingest::download_input;
    use tuliprox_core::model::MaintenanceRegistry;

    let temp = tempfile::tempdir().expect("tempdir");
    let initial_vod = r#"{"js":{"data":[{"id":"101","name":"Movie 1","cmd":"ffmpeg http://stream.example/vod/1","category_id":"10"},{"id":"102","name":"Movie 2","cmd":"ffmpeg http://stream.example/vod/2","category_id":"10"},{"id":"103","name":"Movie 3","cmd":"ffmpeg http://stream.example/vod/3","category_id":"10"},{"id":"104","name":"Movie 4","cmd":"ffmpeg http://stream.example/vod/4","category_id":"10"}],"total_items":4,"max_page_items":10}}"#;
    let vod_body = Arc::new(parking_lot::Mutex::new(initial_vod.to_string()));
    let fail_vod = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (url, server) = start_stalker_test_server(Arc::clone(&vod_body), Arc::clone(&fail_vod)).await;
    let (app, input) = stalker_input_with_quality(temp.path(), url, 90);

    // 1. Seed a clean baseline update so persisted playlist + quality baseline exist.
    let baseline_sink = RecordingSink::default();
    let baseline_ctx = context(Arc::clone(&app), baseline_sink, &[]);
    let baseline = download_input(&baseline_ctx, &input, false).await;
    assert!(baseline.errors.is_empty(), "baseline errors: {:?}", baseline.errors);
    assert_eq!(baseline.quality_rejections.len(), 0);

    // 2. Quality-rejected VOD update with replace_target_contents == false reuses persisted playlist,
    //    but must NOT queue sampling.
    *vod_body.lock() = r#"{"js":{"data":[],"total_items":0,"max_page_items":10}}"#.to_string();
    let qr_sink = RecordingSink::default();
    let qr_queued = Arc::clone(&qr_sink.queued);
    let qr_ctx = context(Arc::clone(&app), qr_sink, &[]);
    let mut qr_result = download_input(&qr_ctx, &input, false).await;
    assert!(!qr_result.quality_rejections.is_empty(), "VOD drop should trigger quality rejection");
    assert!(!qr_result.source.is_empty(), "persisted playlist is reused when replace_target_contents is false");
    queue_container_extension_sampling(&qr_ctx).await;
    assert!(
        qr_queued.lock().is_empty(),
        "quality-rejected update must not queue container extension sampling (queued={:?})",
        *qr_queued.lock()
    );

    // 3. Partial download error (Live succeeds, VOD returns HTTP 500 through MAX_RETRIES so
    //    `download_err` is non-empty while `source` is non-empty and `partial` is false) with
    //    replace_target_contents == false must NOT queue sampling.
    *vod_body.lock() = initial_vod.to_string();
    fail_vod.store(true, std::sync::atomic::Ordering::Release);
    let mut err_ctx = context(Arc::clone(&app), RecordingSink::default(), &[]);
    let mut err_result = download_input(&err_ctx, &input, false).await;
    for _ in 0..3 {
        if !err_result.errors.is_empty() && !err_result.partial {
            break;
        }
        err_ctx = context(Arc::clone(&app), RecordingSink::default(), &[]);
        err_result = download_input(&err_ctx, &input, false).await;
    }
    let err_queued = Arc::clone(&err_ctx.metadata_manager.as_ref().expect("sink").queued);
    assert!(!err_result.errors.is_empty(), "VOD 500 should record download error");
    assert!(!err_result.partial, "exhausted retries should reach terminal error state");
    assert!(!err_result.source.is_empty(), "Live succeeded so playlist is non-empty");
    queue_container_extension_sampling(&err_ctx).await;
    assert!(
        err_queued.lock().is_empty(),
        "update with download errors must not queue container extension sampling (queued={:?})",
        *err_queued.lock()
    );

    // 4. Input in maintenance with persisted playlist must NOT queue sampling.
    fail_vod.store(false, std::sync::atomic::Ordering::Release);
    let maintenance = Arc::new(MaintenanceRegistry::in_memory());
    maintenance.add(&input.name, Some("test".to_string()), 0).await.expect("maintenance");
    let maint_sink = RecordingSink::default();
    let maint_queued = Arc::clone(&maint_sink.queued);
    let mut maint_ctx = context(Arc::clone(&app), maint_sink, &[]);
    maint_ctx.maintenance = Some(maintenance);
    let _maint_result = download_input(&maint_ctx, &input, false).await;
    queue_container_extension_sampling(&maint_ctx).await;
    assert!(
        maint_queued.lock().is_empty(),
        "maintenance run must not queue container extension sampling (queued={:?})",
        *maint_queued.lock()
    );

    server.abort();
}
