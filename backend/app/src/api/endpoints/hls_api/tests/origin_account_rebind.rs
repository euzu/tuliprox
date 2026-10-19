use super::{
    super::{prepare_hls_origin_runtime, HlsOriginWorkClass, HlsOriginWorkKind},
    *,
};
use tuliprox_core::{model::ProviderAccountObservation, utils::current_time_millis};
use tuliprox_hls::api::{
    hls_origin_account_status, HlsOriginAccountStatus, HlsOriginAccountWork, HlsSegmentFile, OriginMapKey,
};

#[derive(Clone, Copy, Debug)]
enum InvalidAccount {
    ExpiredRoot,
    ExpiredAlias,
    RemovedAlias,
    DisabledRoot,
    DisabledAlias,
    RuntimeRoot,
    RuntimeAlias,
}

fn reload_inputs(app: &Arc<AppState>, inputs: Vec<Arc<ConfigInput>>) {
    app.app_config.sources.store(Arc::new(SourcesConfig {
        group_lookup: build_group_lookup(&inputs),
        inputs,
        ..app.app_config.sources.load_full().as_ref().clone()
    }));
    app.active_provider.update_config(&app.app_config);
}

fn invalidate_account(
    app: &Arc<AppState>,
    input: &mut ConfigInput,
    invalid: InvalidAccount,
) -> (&'static str, HlsOriginAccountStatus) {
    match invalid {
        InvalidAccount::ExpiredRoot => input.exp_date = Some(1),
        InvalidAccount::ExpiredAlias => input.aliases.as_mut().unwrap()[0].exp_date = Some(1),
        InvalidAccount::RemovedAlias => input.aliases = None,
        InvalidAccount::DisabledRoot => input.account_disabled = true,
        InvalidAccount::DisabledAlias => input.aliases.as_mut().unwrap()[0].enabled = false,
        InvalidAccount::RuntimeRoot | InvalidAccount::RuntimeAlias => {
            let name = if matches!(invalid, InvalidAccount::RuntimeRoot) {
                &input.name
            } else {
                &input.aliases.as_ref().unwrap()[0].name
            };
            let config = app.active_provider.find_provider_config(name).unwrap();
            assert!(app
                .active_provider
                .observe_account(ProviderAccountObservation {
                    name: Arc::clone(name),
                    identity: config.account_identity(),
                    status: Some(shared::model::ProxyUserStatus::Banned),
                    exp_date: None,
                })
                .is_some());
        }
    }
    if !matches!(invalid, InvalidAccount::RuntimeRoot | InvalidAccount::RuntimeAlias) {
        reload_inputs(app, vec![Arc::new(input.clone())]);
    }
    match invalid {
        InvalidAccount::ExpiredRoot => ("overlap-input", HlsOriginAccountStatus::Expired),
        InvalidAccount::ExpiredAlias => ("account-a", HlsOriginAccountStatus::Expired),
        InvalidAccount::RemovedAlias | InvalidAccount::DisabledAlias => ("account-a", HlsOriginAccountStatus::Missing),
        InvalidAccount::DisabledRoot | InvalidAccount::RuntimeRoot => {
            ("overlap-input", HlsOriginAccountStatus::Blocked)
        }
        InvalidAccount::RuntimeAlias => ("account-a", HlsOriginAccountStatus::Blocked),
    }
}

#[tokio::test]
async fn handover_updates_all_viewers_without_followup_media_requests() {
    let mut input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let proxy_id = session.read().await.proxy_session_id.clone();
    for (port, token) in [(12101, "viewer-a"), (12102, "viewer-b")] {
        super::observability::register_hls_cache_stream_for_stats_test(
            &app,
            &session,
            &proxy_id,
            token,
            &test_fingerprint_with_addr(test_addr_with_port(port)),
            token,
        )
        .await;
    }
    let mut events = app.event_manager.get_event_channel();
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let prepared = prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        &proxy_id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now,
    )
    .await
    .expect("replacement account publishes");
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().account_name.as_ref(), "account-a");
    let rows = app.active_users.panel_streams().await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| row.provider.as_ref() == "account-a"),
        "all viewer rows must change immediately: {rows:?}"
    );
    let mut updated = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let shared::model::EventMessage::ActiveUser(shared::model::ActiveUserConnectionChange::Updated(row)) = event
        {
            updated.push(row);
        }
    }
    assert_eq!(updated.len(), 2);
    assert!(updated.iter().all(|row| row.provider.as_ref() == "account-a"));
    app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
}

#[tokio::test]
async fn invalid_root_alias_matrix_rebinds_within_input_and_preserves_session() {
    for invalid in [
        InvalidAccount::ExpiredRoot,
        InvalidAccount::ExpiredAlias,
        InvalidAccount::RemovedAlias,
        InvalidAccount::DisabledRoot,
        InvalidAccount::DisabledAlias,
        InvalidAccount::RuntimeRoot,
        InvalidAccount::RuntimeAlias,
    ] {
        let mut input = overlap_provider_input();
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let (old, expected_status) = invalidate_account(&app, &mut input, invalid);
        let now = current_time_millis();
        let session = create_bound_hls_test_session(&app, &input, "12345", old, now).await;
        let (key, proxy_id) = {
            let session = session.read().await;
            (session.key.clone(), session.proxy_session_id.clone())
        };
        let old_binding = session.read().await.origin_account_binding.clone().unwrap();
        super::observability::register_hls_cache_stream_for_stats_test(
            &app,
            &session,
            &proxy_id,
            "matrix-viewer",
            &test_fingerprint(),
            "matrix-viewer",
        )
        .await;
        assert_eq!(hls_origin_account_status(&app.hls_ctx(), &old_binding), expected_status, "{invalid:?}");
        let prepared = prepare_hls_origin_runtime(
            &app,
            &session,
            &input,
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            &proxy_id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            now,
        )
        .await
        .unwrap();
        let session = session.read().await;
        let binding = session.origin_account_binding.as_ref().unwrap();
        assert_eq!(
            binding.account_name.as_ref(),
            if old == "account-a" { "overlap-input" } else { "account-a" },
            "{invalid:?}"
        );
        assert_eq!(binding.input_name, input.name);
        assert_eq!(binding.generation, 1);
        assert_eq!((&session.key, &session.proxy_session_id), (&key, &proxy_id));
        assert!(session.origin_account_manifest_pending.is_some());
        assert!(session.origin_control.manifest_origin_binding.is_none());
        assert!(session.origin_provider_session_headers.is_empty());
        assert!(!prepared.fetch_url.contains(if old == "account-a" { "account-user" } else { "root-user" }));
        let expected_provider = binding.account_name.clone();
        drop(session);
        assert!(
            app.active_users.panel_streams().await.iter().all(|row| row.provider == expected_provider),
            "{invalid:?}"
        );
        app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
        assert_eq!(app.active_provider.get_provider_connections_count(), 0);
    }
}

#[tokio::test]
async fn detached_failed_rebind_obeys_two_second_backoff_and_source_boundary() {
    let mut input = overlap_provider_input();
    input.aliases = None;
    let mut other = single_hls_provider_input("unrelated-source");
    other.id = 9;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone()), Arc::new(other.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", "removed-account", now).await;
    let id = session.read().await.proxy_session_id.clone();
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone()), Arc::new(other.clone())]);
    for timestamp in [now, now + 1_999] {
        assert!(prepare_hls_origin_runtime(
            &app,
            &session,
            &input,
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Segment,
            HlsOriginWorkClass::Demand,
            timestamp
        )
        .await
        .is_err());
        assert_eq!(session.read().await.origin_account_rebind.consecutive_rebind_failures, 1);
        assert_eq!(app.active_provider.get_provider_connections_count(), 0);
    }
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone()), Arc::new(other)]);
    let prepared = prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Segment,
        HlsOriginWorkClass::Demand,
        now + 2_000,
    )
    .await
    .unwrap();
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().account_name, input.name);
    app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
    let reused_input = single_hls_provider_input("reused-input-id");
    reload_inputs(&app, vec![Arc::new(reused_input.clone())]);
    assert!(prepare_hls_origin_runtime(
        &app,
        &session,
        &reused_input,
        "http://account.example.com/index.m3u8",
        "http://account.example.com/index.m3u8",
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now + 4_000
    )
    .await
    .is_err());
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().input_name, input.name);
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
    reload_inputs(&app, Vec::new());
    assert!(prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        "http://root.example.com/index.m3u8",
        "http://root.example.com/index.m3u8",
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now + 4_000
    )
    .await
    .is_err());
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().input_name, input.name);
}

#[tokio::test]
async fn concurrent_prepare_publishes_one_replacement() {
    let mut input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let app = Arc::clone(&app);
        let input = input.clone();
        let session = Arc::clone(&session);
        tasks.spawn(async move {
            let id = session.read().await.proxy_session_id.clone();
            let prepared = prepare_hls_origin_runtime(
                &app,
                &session,
                &input,
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                &id,
                &test_fingerprint(),
                ConnectionKind::Normal,
                0,
                HlsOriginWorkKind::Manifest,
                HlsOriginWorkClass::ManifestInteractive,
                now,
            )
            .await
            .unwrap();
            app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().generation, 1);
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
}

pub(super) fn enable_isolated_rebind_cache(app: &Arc<AppState>, path: &std::path::Path) {
    app.app_config.config.store(Arc::new(Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(HlsCacheConfigDto {
                cache_path: Some(path.to_string_lossy().into_owned()),
                ..HlsCacheConfigDto::default()
            }),
            ..ReverseProxyConfigDto::default()
        })),
        ..Config::default()
    }));
}

pub(super) struct RebindOrigin {
    pub(super) base_url: String,
    pub(super) requests: Arc<tokio::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
    block_old_media: Arc<std::sync::atomic::AtomicBool>,
    old_media_started: Arc<tokio::sync::Notify>,
    release_old_media: Arc<tokio::sync::Semaphore>,
}
impl Drop for RebindOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

#[derive(Clone, Copy)]
enum RebindMediaLayout {
    AccountPath,
    SharedPath,
}

async fn rebind_origin(extra_tags: &'static str, layout: RebindMediaLayout) -> RebindOrigin {
    rebind_origin_with_window(extra_tags, layout, None).await
}

pub(super) async fn resume_origin(head: Arc<std::sync::atomic::AtomicU64>) -> RebindOrigin {
    rebind_origin_with_window("", RebindMediaLayout::AccountPath, Some(head)).await
}

pub(super) fn resume_origin_media(path: &str) -> Vec<u8> {
    // Valid TS, distinct from the enabled channel-unavailable asset. A null packet
    // identifies the exact origin URI as well, preventing old cache hits from passing.
    let mut media =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_connections_exhausted.ts"))
            .to_vec();
    let mut marker = [0xff; 188];
    marker[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    marker[4..4 + path.len()].copy_from_slice(path.as_bytes());
    media.extend_from_slice(&marker);
    media
}

async fn rebind_origin_with_window(
    extra_tags: &'static str,
    layout: RebindMediaLayout,
    live_head: Option<Arc<std::sync::atomic::AtomicU64>>,
) -> RebindOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let block_old_media = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let old_media_started = Arc::new(tokio::sync::Notify::new());
    let release_old_media = Arc::new(tokio::sync::Semaphore::new(0));
    let (block, started, release) =
        (Arc::clone(&block_old_media), Arc::clone(&old_media_started), Arc::clone(&release_old_media));
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let requests = Arc::clone(&recorded);
            let live_head = live_head.clone();
            let (block, started, release) = (Arc::clone(&block), Arc::clone(&started), Arc::clone(&release));
            tokio::spawn(async move {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_string();
                let alias_cookie = request.to_ascii_lowercase().contains("origin-account=alias");
                requests.lock().await.push(request);
                if block.load(Ordering::Acquire) && path.contains("root-user") && path.contains(".ts") {
                    started.notify_one();
                    release.acquire().await.unwrap().forget();
                }
                let replacement = path.contains("account-user");
                let body = if let Some(head) = live_head {
                    if path.ends_with("m3u8") {
                        super::manifest_entry_freshness::origin_playlist_with_duration(head.load(Ordering::Acquire), 11)
                    } else {
                        resume_origin_media(&path)
                    }
                } else if path.ends_with("m3u8") {
                    let (head, count) = match (layout, replacement) {
                        (RebindMediaLayout::AccountPath, true) => (108, 8),
                        (RebindMediaLayout::SharedPath, true) => (102, 7),
                        (RebindMediaLayout::AccountPath | RebindMediaLayout::SharedPath, false) => (100, 8),
                    };
                    let mut manifest =
                        format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
                    manifest.push_str(extra_tags);
                    for seq in head..head + count {
                        let uri = match layout {
                            RebindMediaLayout::AccountPath => format!("{seq}.ts"),
                            RebindMediaLayout::SharedPath => format!(
                                "/common/{seq}.ts?account={}",
                                if replacement { "account-user" } else { "root-user" }
                            ),
                        };
                        writeln!(manifest, "#EXTINF:4,\n{uri}").unwrap();
                    }
                    manifest.into_bytes()
                } else if path == "/shared/key.bin" {
                    vec![if alias_cookie { 0x41 } else { 0x52 }; 16]
                } else if path.contains(".ts") {
                    include_bytes!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../test/fixtures/hls/channel_unavailable.ts"
                    ))
                    .to_vec()
                } else {
                    vec![0x47; 8192]
                };
                let cookie = if replacement { "alias" } else { "root" };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSet-Cookie: origin-account={cookie}; Path=/\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });
    RebindOrigin { base_url, requests, task, block_old_media, old_media_started, release_old_media }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn segment_and_background_work_fetch_new_manifest_and_media_without_client_manifest() {
    for (background, cross_host, provider_scheme) in
        [(false, false, false), (true, false, false), (false, true, false), (true, false, true)]
    {
        let origin =
            rebind_origin("", if cross_host { RebindMediaLayout::SharedPath } else { RebindMediaLayout::AccountPath })
                .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let mut input = overlap_provider_input();
        input.url.clone_from(&origin.base_url);
        input.aliases.as_mut().unwrap()[0].url =
            if cross_host { origin.base_url.replace("127.0.0.1", "localhost") } else { origin.base_url.clone() };
        if provider_scheme {
            let provider = Arc::new(crate::model::ConfigProvider::from(&shared::model::ConfigProviderDto {
                name: "rebind-mirror".into(),
                urls: vec![origin.base_url.clone().into()],
                provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::ResumeLastWorking,
                dns: None,
            }));
            input.url = "provider://rebind-mirror".to_string();
            input.aliases.as_mut().unwrap()[0].url.clone_from(&input.url);
            input.provider_configs = Some(vec![provider]);
        }
        let app = test_app_state_with_hls_proxy_and_inputs(
            Arc::new(HlsProxyManager::with_cache_settings(cache_dir.path(), 300)),
            vec![Arc::new(input.clone())],
        );
        enable_isolated_rebind_cache(&app, cache_dir.path());
        app.hls.proxy.update_config(&app.app_config).await;
        let now = current_time_millis();
        let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
        let id = session.read().await.proxy_session_id.clone();
        let lease_uri = hls_proxy_uri(&app, &id.0, "000000.ts").await;
        let url = format!("{}/live/root-user/root-pass/12345.m3u8", input.url);
        let initial = prepare_hls_origin_runtime(
            &app,
            &session,
            &input,
            &url,
            &url,
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            now,
        )
        .await
        .unwrap();
        app.connection_manager.release_managed_provider_handle(initial.preacquired_origin_account_handle);
        session.write().await.origin_account_manifest_pending = Some(0);
        let runtime = session.read().await.origin_account_runtime.clone().unwrap();
        runtime.prepare(&session, HlsOriginAccountWork::Media, HlsOriginWorkClass::Demand).await.unwrap();
        // Wait for initial prefetches and retain one cached object as identity/cache evidence.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if session.read().await.active_segment_fetches == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for seq in 0..8 {
            let context =
                super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
            let file = HlsSegmentFile { proxy_seq: seq, extension: "ts".to_string() };
            let outcome =
                app.hls.proxy.segment_worker_pool().demand_fetch_and_wait(context, &file, current_time_millis()).await;
            assert_eq!(outcome, tuliprox_hls::api::SegmentDemandFetchOutcome::Ready);
        }
        session
            .write()
            .await
            .render_and_store_manifest(current_time_millis())
            .expect("initial ready origin window is published");
        assert!(session.read().await.established_manifest_recovery_binding().is_some());
        let (key, cached, target_seq) = {
            let mut s = session.write().await;
            let cached = s
                .segments
                .iter()
                .find_map(|(seq, entry)| matches!(entry.status, SegmentCacheStatus::Ready { .. }).then_some(*seq));
            let seq = *s.segments.keys().next_back().unwrap();
            let cache_key = s.segments[&seq].cache_key.clone();
            s.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
            s.origin_provider_session_headers
                .insert(header::COOKIE, HeaderValue::from_static("old-account=must-disappear"));
            // Prior hard failure must not send recovery back to root credentials.
            s.require_fresh_manifest_commit(
                tuliprox_hls::api::HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
            );
            s.origin_refresh.consecutive_failures = 1;
            s.origin_refresh.next_fetch_allowed_at_ms = 0;
            (s.key.clone(), cached, (seq, cache_key))
        };
        app.hls.proxy.segment_cache().delete(&target_seq.1).await.unwrap();
        input.account_disabled = true;
        reload_inputs(&app, vec![Arc::new(input.clone())]);
        let boundary = origin.requests.lock().await.len();
        if background {
            session.write().await.queue_segment_fetch_candidate(
                target_seq.0,
                tuliprox_hls::api::SegmentFetchPriority::Prefetch,
                current_time_millis(),
            );
            let context =
                super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
            app.hls.proxy.segment_worker_pool().wake_scheduler(context, current_time_millis()).await;
        } else {
            let uri = lease_uri.replace("000000.ts", &format!("{:06}.ts", target_seq.0));
            let response = get_response(Arc::clone(&app), &uri, None).await;
            assert!(
                matches!(response.status(), StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE),
                "{}",
                response.status()
            );
        }
        let replacement_ready = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = session.read().await.segments.iter().find_map(|(seq, entry)| {
                    (matches!(entry.status, SegmentCacheStatus::Ready { .. })
                        && entry
                            .origin_fetch_ref
                            .as_ref()
                            .is_some_and(|reference| reference.resolved_origin_url.contains("account-user")))
                    .then_some(*seq)
                });
                if let Some(seq) = ready {
                    break seq;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let new_proxy_seq = match replacement_ready {
            Ok(seq) => seq,
            Err(error) => {
                let s = session.read().await;
                panic!("replacement media: background={background} host={cross_host} provider={provider_scheme}: {error}; pending={:?} refresh={:?} episode={:?} requests={:?}", s.origin_account_manifest_pending, s.origin_refresh, s.origin_control.acceptance_episode.as_ref().map(|episode| (&episode.state, episode.full_burst_completed)), origin.requests.lock().await.iter().map(|request| request.lines().next().unwrap()).collect::<Vec<_>>());
            }
        };
        let target = HlsSegmentFile { proxy_seq: new_proxy_seq, extension: "ts".to_string() };
        let context =
            super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
        let outcome =
            app.hls.proxy.segment_worker_pool().demand_fetch_and_wait(context, &target, current_time_millis()).await;
        assert_eq!(
            outcome,
            tuliprox_hls::api::SegmentDemandFetchOutcome::Ready,
            "background={background} cross_host={cross_host} provider={provider_scheme}"
        );
        let new_uri = lease_uri.replace("000000.ts", &format!("{new_proxy_seq:06}.ts"));
        let response = get_response(Arc::clone(&app), &new_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_body(response).await.as_ref(),
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
        );
        let s = session.read().await;
        assert_eq!((&s.key, &s.proxy_session_id), (&key, &id));
        assert_eq!(s.origin_account_binding.as_ref().unwrap().account_name.as_ref(), "account-a");
        assert_eq!(s.origin_account_binding.as_ref().unwrap().generation, 1);
        assert!(s.origin_account_manifest_pending.is_none());
        if let Some(cached) = cached {
            if cached != target_seq.0 {
                assert!(matches!(s.segments[&cached].status, SegmentCacheStatus::Ready { .. }));
            }
        }
        drop(s);
        let requests = origin.requests.lock().await;
        let replacement = &requests[boundary..];
        assert!(
            replacement.iter().any(|request| request.starts_with("GET /live/account-user/account-pass/12345.m3u8 ")),
            "{replacement:?}"
        );
        assert!(
            replacement.iter().any(|request| request.lines().next().unwrap().contains("account-user")
                && !request.lines().next().unwrap().contains("m3u8")),
            "{replacement:?}"
        );
        assert!(
            replacement.iter().all(|request| !request.contains("root-user") && !request.contains("old-account=")),
            "{replacement:?}"
        );
    }
}

#[tokio::test]
async fn independent_sessions_share_source_capacity_and_leave_other_source_unchanged() {
    let mut input = overlap_provider_input();
    let mut alias = input.aliases.as_ref().unwrap()[0].clone();
    alias.id = 3;
    alias.name = Arc::from("account-b");
    input.aliases.as_mut().unwrap().push(alias);
    let mut other = single_hls_provider_input("other-source");
    other.id = 8;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone()), Arc::new(other.clone())]);
    let now = current_time_millis();
    let first = create_bound_hls_test_session(&app, &input, "first", &input.name, now).await;
    let second = create_bound_hls_test_session(&app, &input, "second", &input.name, now).await;
    let unaffected = create_bound_hls_test_session(&app, &other, "third", &other.name, now).await;
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone()), Arc::new(other.clone())]);
    let mut handles = Vec::new();
    for session in [&first, &second] {
        let id = session.read().await.proxy_session_id.clone();
        handles.push(
            prepare_hls_origin_runtime(
                &app,
                session,
                &input,
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                &id,
                &test_fingerprint(),
                ConnectionKind::Normal,
                0,
                HlsOriginWorkKind::Manifest,
                HlsOriginWorkClass::ManifestInteractive,
                now,
            )
            .await
            .unwrap(),
        );
    }
    assert_ne!(
        first.read().await.origin_account_binding.as_ref().unwrap().account_name,
        second.read().await.origin_account_binding.as_ref().unwrap().account_name
    );
    assert_eq!(app.active_provider.get_provider_connections_count(), 2);
    let unaffected = unaffected.read().await;
    assert_eq!(unaffected.origin_account_binding.as_ref().unwrap().account_name, other.name);
    assert_eq!(unaffected.origin_account_binding.as_ref().unwrap().generation, 0);
    drop(handles);
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn healthy_full_account_keeps_binding_and_cancelled_prepare_leaks_no_slot() {
    let input = single_hls_provider_input("healthy");
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let occupied = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_addr(),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("foreign-owner", crate::model::PlaybackKind::LiveHls)),
        )
        .unwrap();
    let id = session.read().await.proxy_session_id.clone();
    let prepared = prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now,
    )
    .await
    .unwrap();
    assert!(prepared.preacquired_origin_account_handle.is_none());
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().generation, 0);
    assert_eq!(session.read().await.origin_account_rebind.last_rebind_attempt_at_ms, None);
    app.connection_manager.release_provider_handle(Some(occupied));
    let config = app.active_provider.find_provider_config(&input.name).unwrap();
    app.active_provider.observe_account(ProviderAccountObservation {
        name: Arc::clone(&input.name),
        identity: config.account_identity(),
        status: Some(shared::model::ProxyUserStatus::Banned),
        exp_date: None,
    });
    let lock = Arc::clone(&session.read().await.origin_account_prepare_lock);
    let guard = lock.lock().await;
    let task_session = Arc::clone(&session);
    let task_app = Arc::clone(&app);
    let task = tokio::spawn(async move {
        prepare_hls_origin_runtime(
            &task_app,
            &task_session,
            &input,
            "http://account.example.com/index.m3u8",
            "http://account.example.com/index.m3u8",
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Segment,
            HlsOriginWorkClass::Demand,
            now,
        )
        .await
    });
    tokio::task::yield_now().await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    drop(guard);
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
}

async fn resource_rebind_fixture(
    extra_tags: &'static str,
) -> (tempfile::TempDir, RebindOrigin, Arc<AppState>, ConfigInput, HlsSessionHandle) {
    let origin = rebind_origin(extra_tags, RebindMediaLayout::AccountPath).await;
    let dir = tempfile::tempdir().unwrap();
    let mut input = overlap_provider_input();
    input.url.clone_from(&origin.base_url);
    input.aliases.as_mut().unwrap()[0].url.clone_from(&origin.base_url);
    let app = test_app_state_with_hls_proxy_and_inputs(
        Arc::new(HlsProxyManager::with_cache_settings(dir.path(), 300)),
        vec![Arc::new(input.clone())],
    );
    enable_isolated_rebind_cache(&app, dir.path());
    app.hls.proxy.update_config(&app.app_config).await;
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let id = session.read().await.proxy_session_id.clone();
    let _uri = hls_proxy_uri(&app, &id.0, "000000.ts").await;
    let url = format!("{}/live/root-user/root-pass/12345.m3u8", input.url);
    let prepared = prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        &url,
        &url,
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now,
    )
    .await
    .unwrap();
    drop(prepared);
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    (dir, origin, app, input, session)
}

#[tokio::test]
async fn map_worker_initiates_rebind_and_fetches_requalified_map() {
    let (_dir, origin, app, _input, session) = resource_rebind_fixture("#EXT-X-MAP:URI=\"init.mp4\"\n").await;
    let id = session.read().await.proxy_session_id.clone();
    let old_map = crate::api::model::MapEntry::new(
        &id,
        ProxyMapId(90),
        OriginMapKey {
            origin_epoch: 0,
            resolved_origin_uri: format!("{}/live/root-user/root-pass/old.mp4", origin.base_url),
            byte_range: None,
        },
        "mp4".to_string(),
    );
    session.write().await.maps.insert(ProxyMapId(90), old_map);
    let segment_context =
        super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
    let context = tuliprox_hls::api::MapFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::clone(app.hls.proxy.segment_cache()),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::new(),
        use_manual_redirects: false,
        origin_io: segment_context.origin_io,
    };
    app.hls.proxy.map_worker_pool().wake_scheduler(context, current_time_millis()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if session.read().await.maps.values().any(|map| {
                map.origin_key.resolved_origin_uri.contains("account-user")
                    && matches!(map.status, MapCacheStatus::Ready { .. })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement manifest schedules its new MAP");
    assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().generation, 1);
    let requests = origin.requests.lock().await;
    assert!(
        requests.iter().any(|request| request.starts_with("GET /live/account-user/account-pass/init.mp4 ")),
        "{requests:?}"
    );
    assert!(
        requests.iter().all(|request| !request.contains("root-user") && !request.contains("old-account=")),
        "{requests:?}"
    );
}

#[tokio::test]
async fn transient_key_and_media_misses_rebind_before_reusing_any_old_url() {
    for (kind, shared_uri) in [
        (TransientResourceKind::Key, false),
        (TransientResourceKind::Segment, false),
        (TransientResourceKind::Key, true),
    ] {
        let tags = if shared_uri {
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"/shared/key.bin\"\n"
        } else {
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n"
        };
        let (_dir, origin, app, _input, session) = resource_rebind_fixture(tags).await;
        let now = current_time_millis();
        let resource = TransientResourceRef::new(
            kind,
            if shared_uri {
                format!("{}/shared/key.bin", origin.base_url)
            } else {
                format!("{}/live/root-user/root-pass/old.bin", origin.base_url)
            },
            &app.get_encrypt_secret(),
            now,
            300_000,
            Some("bin".to_string()),
        );
        let id = session.read().await.proxy_session_id.clone();
        {
            let mut s = session.write().await;
            s.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            s.transient.upsert_resources([resource.clone()]);
            s.origin_provider_session_headers
                .insert(header::COOKIE, HeaderValue::from_static("old-account=must-disappear"));
        }
        let uri = hls_proxy_uri(&app, &id.0, &format!("r/{}.bin", resource.id.0)).await;
        let response = get_response(Arc::clone(&app), &uri, None).await;
        assert!(
            response.status() == StatusCode::SERVICE_UNAVAILABLE || response.status() == StatusCode::NOT_FOUND,
            "{}",
            response.status()
        );
        assert!(response.headers().contains_key(header::RETRY_AFTER) || response.status() == StatusCode::NOT_FOUND);
        let resource_id = resource.id.clone();
        let new_resource = session
            .read()
            .await
            .transient
            .resources
            .values()
            .find(|resource| {
                resource.kind == kind
                    && (resource.resolved_origin_uri.contains("account-user")
                        || (shared_uri && resource.id != resource_id))
            })
            .cloned()
            .expect("new manifest publishes replacement resources");
        let extension = new_resource.file_ext_hint.as_deref().unwrap();
        let uri = hls_proxy_uri(&app, &id.0, &format!("r/{}.{extension}", new_resource.id.0)).await;
        let response = get_response(Arc::clone(&app), &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let expected: &[u8] = if kind == TransientResourceKind::Segment {
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
        } else if shared_uri {
            &[0x41; 16]
        } else {
            &[0x47; 8192]
        };
        assert_eq!(response_body(response).await.as_ref(), expected);
        assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().generation, 1);
        let requests = origin.requests.lock().await;
        assert!(requests.iter().any(|request| !request.lines().next().unwrap().contains("m3u8")), "{requests:?}");
        assert!(
            requests.iter().all(|request| !request.contains("root-user") && !request.contains("old-account=")),
            "{requests:?}"
        );
    }
}

#[tokio::test]
async fn abort_after_allocating_replacement_releases_its_handle() {
    let mut input = overlap_provider_input();
    input.account_disabled = true;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, current_time_millis()).await;
    let (allocated_tx, allocated_rx) = tokio::sync::oneshot::channel();
    let task_app = Arc::clone(&app);
    let task = tokio::spawn(async move {
        let id = session.read().await.proxy_session_id.clone();
        let prepared = prepare_hls_origin_runtime(
            &task_app,
            &session,
            &input,
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            current_time_millis(),
        )
        .await
        .unwrap();
        allocated_tx.send(()).unwrap();
        futures::future::pending::<()>().await;
        drop(prepared);
    });
    allocated_rx.await.unwrap();
    assert_eq!(app.active_provider.get_provider_connections_count(), 1);
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn late_origin_response_and_cleanup_cannot_overwrite_replacement() {
    let (_dir, origin, app, mut input, session) = resource_rebind_fixture("").await;
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let manifest = normal_manifest(&format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n{}/live/root-user/root-pass/100.ts\n",
        origin.base_url
    ));
    session.write().await.apply_origin_manifest(&manifest).unwrap();
    origin.block_old_media.store(true, Ordering::Release);
    let context = super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
    let worker = Arc::clone(app.hls.proxy.segment_worker_pool());
    let old_request = tokio::spawn(async move {
        worker
            .demand_fetch_and_wait(
                context,
                &HlsSegmentFile { proxy_seq: 0, extension: "ts".to_string() },
                current_time_millis(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), origin.old_media_started.notified())
        .await
        .expect("old request reaches controlled barrier");
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input)]);
    let runtime = session.read().await.origin_account_runtime.clone().unwrap();
    runtime.prepare(&session, HlsOriginAccountWork::Media, HlsOriginWorkClass::Demand).await.unwrap();
    let replacement = session.read().await.origin_account_binding.clone().unwrap();
    let boundary = origin.requests.lock().await.len();
    origin.release_old_media.add_permits(1);
    let _old_outcome = tokio::time::timeout(Duration::from_secs(5), old_request).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = session.read().await;
            if s.active_segment_fetches == 0 && s.activity.active_origin_work_count.load(Ordering::Acquire) == 0 {
                break;
            }
            drop(s);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let s = session.read().await;
    let binding = s.origin_account_binding.as_ref().unwrap();
    assert_eq!((&binding.account_name, binding.generation), (&replacement.account_name, replacement.generation));
    assert!(binding.is_active());
    assert!(s.segments.iter().any(|(seq, entry)| *seq > 0 && matches!(entry.status, SegmentCacheStatus::Ready { .. })));
    assert!(!matches!(s.segments[&0].status, SegmentCacheStatus::Ready { .. }));
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
    assert!(origin.requests.lock().await[boundary..].iter().all(|request| !request.contains("root-user")));
}

struct BlockAccountBeforeAcquire {
    delegate: Arc<dyn tuliprox_hls::api::HlsOriginAccountRuntime>,
    app: std::sync::Weak<AppState>,
    account: Arc<str>,
    calls: AtomicUsize,
}
impl std::fmt::Debug for BlockAccountBeforeAcquire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockAccountBeforeAcquire").finish_non_exhaustive()
    }
}
impl tuliprox_hls::api::HlsOriginAccountRuntime for BlockAccountBeforeAcquire {
    fn prepare<'a>(
        &'a self,
        session: &'a HlsSessionHandle,
        work: HlsOriginAccountWork,
        class: HlsOriginWorkClass,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        tuliprox_hls::api::HlsPreparedOriginAccount,
                        tuliprox_hls::api::HlsBoundAccountAcquireErrorKind,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let prepared = self.delegate.prepare(session, work, class).await?;
            if self.calls.fetch_add(1, Ordering::AcqRel) == 1 {
                let app = self.app.upgrade().unwrap();
                let provider = app.active_provider.find_provider_config(&self.account).unwrap();
                app.active_provider.observe_account(ProviderAccountObservation {
                    name: Arc::clone(&self.account),
                    identity: provider.account_identity(),
                    status: Some(shared::model::ProxyUserStatus::Banned),
                    exp_date: None,
                });
            }
            Ok(prepared)
        })
    }
}

#[tokio::test]
async fn account_blocked_between_prepare_and_acquire_still_rebinds_from_media_work() {
    let (_dir, origin, app, mut input, session) = resource_rebind_fixture("").await;
    let mut config = app.app_config.config.load_full().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().max_concurrent_segment_fetches_global = 1;
    app.app_config.config.store(Arc::new(config));
    app.hls.proxy.update_config(&app.app_config).await;
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let delegate = session.read().await.origin_account_runtime.clone().unwrap();
    session.write().await.origin_account_runtime = Some(Arc::new(BlockAccountBeforeAcquire {
        delegate,
        app: Arc::downgrade(&app),
        account: Arc::clone(&input.name),
        calls: AtomicUsize::new(0),
    }));
    let manifest = normal_manifest(&format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n{}/live/root-user/root-pass/100.ts\n",
        origin.base_url
    ));
    session.write().await.apply_origin_manifest(&manifest).unwrap();
    let context = super::super::build_hls_segment_fetch_context(&app, &session, None, &test_fingerprint(), None).await;
    let _old_outcome = app
        .hls
        .proxy
        .segment_worker_pool()
        .demand_fetch_and_wait(
            context,
            &HlsSegmentFile { proxy_seq: 0, extension: "ts".to_string() },
            current_time_millis(),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = session.read().await;
            if s.origin_account_binding.as_ref().is_some_and(|binding| binding.account_name.as_ref() == "account-a")
                && s.segments.values().any(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
            {
                break;
            }
            drop(s);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("confirmed invalidity at acquisition initiates replacement");
    let requests = origin.requests.lock().await;
    assert!(requests
        .iter()
        .any(|request| request.contains("account-user") && request.lines().next().unwrap().contains(".ts")));
    assert!(requests.iter().all(|request| !request.contains("root-user")));
}

#[tokio::test]
async fn identical_key_uri_caches_distinct_account_bytes_and_keeps_published_old_key() {
    let (_dir, origin, app, mut input, session) =
        resource_rebind_fixture("#EXT-X-KEY:METHOD=AES-128,URI=\"/shared/key.bin\"\n").await;
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let old_key = TransientResourceRef::new(
        TransientResourceKind::Key,
        format!("{}/shared/key.bin", origin.base_url),
        &app.get_encrypt_secret(),
        now,
        300_000,
        Some("bin".to_string()),
    );
    let id = {
        let mut current = session.write().await;
        current.transient.upsert_resources([old_key.clone()]);
        current.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("origin-account=root"));
        current.proxy_session_id.clone()
    };
    let old_uri = hls_proxy_uri(&app, &id.0, &format!("r/{}.bin", old_key.id.0)).await;
    let response = get_response(Arc::clone(&app), &old_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await.as_ref(), &[0x52; 16]);
    assert!(session
        .read()
        .await
        .transient
        .ready_key_object_valid_until_ms(&id, &old_key.id, "bin", current_time_millis())
        .is_some());
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input)]);
    let runtime = session.read().await.origin_account_runtime.clone().unwrap();
    runtime.prepare(&session, HlsOriginAccountWork::Media, HlsOriginWorkClass::Demand).await.unwrap();
    let new_key = session
        .read()
        .await
        .transient
        .resources
        .values()
        .find(|resource| {
            resource.kind == TransientResourceKind::Key
                && resource.id != old_key.id
                && resource.resolved_origin_uri == old_key.resolved_origin_uri
        })
        .cloned()
        .expect("new account publishes a separate key identity for the same URI");
    let extension = new_key.file_ext_hint.as_deref().unwrap();
    let prefix = old_uri.rsplit_once("/r/").unwrap().0;
    let new_uri = format!("{prefix}/r/{}.{extension}", new_key.id.0);
    let lease_id = old_uri.split('/').nth(5).unwrap();
    super::support::publish_test_transient_resource_membership(&app, &id.0, lease_id, &format!("{old_uri}\n{new_uri}"))
        .await;
    let response = get_response(Arc::clone(&app), &new_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await.as_ref(), &[0x41; 16]);
    let request_count =
        origin.requests.lock().await.iter().filter(|request| request.starts_with("GET /shared/key.bin ")).count();
    let response = get_response(Arc::clone(&app), &old_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await.as_ref(), &[0x52; 16]);
    assert_eq!(
        origin.requests.lock().await.iter().filter(|request| request.starts_with("GET /shared/key.bin ")).count(),
        request_count,
        "old published key remains a cache hit"
    );
}

#[tokio::test]
async fn manifest_request_built_after_a_handoff_uses_the_snapshot_entry_and_reprepares_it() {
    use super::super::origin_refresh_request::{hls_origin_refresh_request, HlsOriginRefreshOptions};
    let (_dir, origin, app, mut input, session) = resource_rebind_fixture("").await;
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    session.write().await.replace_origin_account_binding(None);
    let id = session.read().await.proxy_session_id.clone();
    let root_url = format!("{}/live/root-user/root-pass/12345.m3u8", origin.base_url);
    let prepared = prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        &root_url,
        &root_url,
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        current_time_millis(),
    )
    .await
    .unwrap();
    // Preserve the exact result across a controlled asynchronous boundary.
    let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
    let task_app = Arc::clone(&app);
    let task_session = Arc::clone(&session);
    let task = tokio::spawn(async move {
        continue_rx.await.unwrap();
        let mut io = super::provider_admission::test_hls_origin_io_context(&task_app);
        io.account_runtime = task_session.read().await.origin_account_runtime.clone();
        if let Some(handle) = prepared.preacquired_origin_account_handle {
            io = io.with_managed_provider_handle(handle);
        }
        let request = hls_origin_refresh_request(
            &task_app,
            &task_session,
            tuliprox_hls::api::HlsPreparedOriginAccount {
                entry: tuliprox_hls::api::LiveHlsOriginEntry::parse(&prepared.fetch_url).unwrap(),
                snapshot: prepared.snapshot,
            },
            HlsOriginRefreshOptions {
                headers: HeaderMap::new(),
                origin_io: io,
                manifest_commit_requirement: tuliprox_hls::api::HlsManifestCommitRequirement::CommittedManifestAllowed,
                acceptance_directive: tuliprox_hls::api::HlsManifestAcceptanceDirective::none(),
                access_lease_id: None,
                rewrite_secret: task_app.get_encrypt_secret().to_vec(),
                now_ms: current_time_millis(),
            },
        );
        tuliprox_hls::api::trigger_origin_refresh_sync(request).await
    });
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    drop(
        prepare_hls_origin_runtime(
            &app,
            &session,
            &input,
            &root_url,
            &root_url,
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            current_time_millis(),
        )
        .await
        .unwrap(),
    );
    continue_tx.send(()).unwrap();
    assert!(task.await.unwrap());
    let requests = origin.requests.lock().await;
    assert!(requests.iter().any(|request| request.starts_with("GET /live/account-user/account-pass/12345.m3u8 ")));
    assert!(requests.iter().all(|request| !request.contains("root-user")), "{requests:?}");
}

struct PausedOriginPrepare {
    delegate: Arc<dyn tuliprox_hls::api::HlsOriginAccountRuntime>,
    prepared: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl std::fmt::Debug for PausedOriginPrepare {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PausedOriginPrepare").finish_non_exhaustive()
    }
}

impl tuliprox_hls::api::HlsOriginAccountRuntime for PausedOriginPrepare {
    fn prepare<'a>(
        &'a self,
        session: &'a HlsSessionHandle,
        work: HlsOriginAccountWork,
        class: HlsOriginWorkClass,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        tuliprox_hls::api::HlsPreparedOriginAccount,
                        tuliprox_hls::api::HlsBoundAccountAcquireErrorKind,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let prepared = self.delegate.prepare(session, work, class).await?;
            self.prepared.notify_one();
            self.release.acquire().await.unwrap().forget();
            Ok(prepared)
        })
    }
}

#[tokio::test]
async fn runtime_prepare_result_superseded_before_refresh_start_cannot_authorize_its_old_entry() {
    use super::super::origin_refresh_request::{hls_origin_refresh_request, HlsOriginRefreshOptions};
    let (_dir, origin, app, mut input, session) = resource_rebind_fixture("").await;
    input.account_disabled = false;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let runtime = Arc::new(PausedOriginPrepare {
        delegate: session.read().await.origin_account_runtime.clone().unwrap(),
        prepared: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut io = super::provider_admission::test_hls_origin_io_context(&app);
    io.account_runtime = Some(runtime.clone());
    let entry = tuliprox_hls::api::LiveHlsOriginEntry::parse(&format!(
        "{}/live/root-user/root-pass/12345.m3u8",
        origin.base_url
    ))
    .unwrap();
    let request = hls_origin_refresh_request(
        &app,
        &session,
        tuliprox_hls::api::HlsPreparedOriginAccount {
            entry,
            snapshot: tuliprox_hls::api::HlsOriginAccountSnapshot::from_session(&*session.read().await),
        },
        HlsOriginRefreshOptions {
            headers: HeaderMap::new(),
            origin_io: io,
            manifest_commit_requirement: tuliprox_hls::api::HlsManifestCommitRequirement::CommittedManifestAllowed,
            acceptance_directive: tuliprox_hls::api::HlsManifestAcceptanceDirective::none(),
            access_lease_id: None,
            rewrite_secret: app.get_encrypt_secret().to_vec(),
            now_ms: current_time_millis(),
        },
    );
    // Force a new prepare and pause its actual app-adapter result before the start CAS.
    let mut request = request;
    request.origin_io.as_mut().unwrap().prepared_account = None;
    let refresh = tokio::spawn(tuliprox_hls::api::maybe_trigger_origin_refresh_with_outcome(request));
    tokio::time::timeout(Duration::from_secs(5), runtime.prepared.notified()).await.unwrap();
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let id = session.read().await.proxy_session_id.clone();
    let url = format!("{}/live/root-user/root-pass/12345.m3u8", origin.base_url);
    drop(
        prepare_hls_origin_runtime(
            &app,
            &session,
            &input,
            &url,
            &url,
            &id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            current_time_millis(),
        )
        .await
        .unwrap(),
    );
    runtime.release.add_permits(1);
    assert_eq!(refresh.await.unwrap(), tuliprox_hls::api::HlsOriginRefreshTriggerOutcome::SessionUnavailable);
    assert!(origin.requests.lock().await.is_empty(), "superseded runtime result sends no origin request");
    assert!(!session.read().await.origin_refresh.in_flight);
}

#[tokio::test]
async fn failed_rebind_does_not_publish_replacement_provider() {
    let mut input = overlap_provider_input();
    input.aliases = None;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let id = session.read().await.proxy_session_id.clone();
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "viewer",
        &test_fingerprint(),
        "viewer",
    )
    .await;
    let before = app.active_users.panel_streams().await;
    let mut events = app.event_manager.get_event_channel();
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    assert!(prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        &id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now
    )
    .await
    .is_err());
    assert_eq!(app.active_users.panel_streams().await, before);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event,
            shared::model::EventMessage::ActiveUser(shared::model::ActiveUserConnectionChange::Updated(_))
        ));
    }
}

#[tokio::test]
async fn older_handover_cannot_overwrite_newer_provider() {
    use super::super::origin_account_runtime::publish_hls_origin_account_with_status;
    use tuliprox_hls::api::HlsOriginAccountSnapshot;
    let input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let id = session.read().await.proxy_session_id.clone();
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "viewer",
        &test_fingerprint(),
        "viewer",
    )
    .await;
    let original = session.read().await.origin_account_binding.clone().unwrap();
    let publish = |name: &'static str, generation| {
        HlsOriginAccountBinding::rebound(
            input.name.clone(),
            Arc::from(name),
            original.session_owner.clone(),
            generation,
            now,
        )
    };
    let binding_b = publish("account-b", 1);
    let snapshot_b = publish_hls_origin_account_with_status(&app, &session, |current| {
        current.handoff_origin_account(binding_b);
        Ok(HlsOriginAccountSnapshot::from_session(current))
    })
    .await
    .unwrap();
    let ready = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let old = {
        let app = app.clone();
        let session = session.clone();
        let ready = ready.clone();
        let resume = resume.clone();
        tokio::spawn(async move {
            ready.notify_one();
            resume.notified().await;
            publish_hls_origin_account_with_status(&app, &session, |_| Ok(snapshot_b)).await
        })
    };
    ready.notified().await;
    let binding_c = publish("account-c", 2);
    publish_hls_origin_account_with_status(&app, &session, |current| {
        current.handoff_origin_account(binding_c);
        Ok(HlsOriginAccountSnapshot::from_session(current))
    })
    .await
    .unwrap();
    let mut events = app.event_manager.get_event_channel();
    resume.notify_one();
    assert!(old.await.unwrap().is_err());
    assert_eq!(app.active_users.panel_streams().await[0].provider.as_ref(), "account-c");
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn stale_media_registration_cannot_restore_previous_provider() {
    use super::super::origin_account_runtime::publish_hls_origin_account_with_status;
    use tuliprox_hls::api::HlsOriginAccountSnapshot;
    let input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, current_time_millis()).await;
    let id = session.read().await.proxy_session_id.clone();
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "viewer",
        &test_fingerprint(),
        "viewer",
    )
    .await;
    let gate = session.read().await.origin_account_prepare_lock.clone();
    let held = gate.lock().await;
    let fp = test_fingerprint();
    let headers = HeaderMap::new();
    let access = test_hls_access_context_with(id.clone(), HlsAccessLeaseId("viewer".into()), "viewer", fp.key.clone());
    let mut registration =
        Box::pin(super::super::ensure_hls_cache_stream_registered(&app, &fp, &headers, &access, &session));
    assert!(futures::poll!(registration.as_mut()).is_pending());
    publish_hls_origin_account_with_status(&app, &session, |current| {
        let old = current.origin_account_binding.as_ref().unwrap();
        let next = HlsOriginAccountBinding::rebound(
            input.name.clone(),
            Arc::from("account-b"),
            old.session_owner.clone(),
            old.generation + 1,
            current_time_millis(),
        );
        current.handoff_origin_account(next);
        Ok(HlsOriginAccountSnapshot::from_session(current))
    })
    .await
    .unwrap();
    drop(held);
    assert_eq!(registration.await.unwrap().provider.as_ref(), "account-b");
    assert_eq!(app.active_users.panel_streams().await[0].provider.as_ref(), "account-b");
}

#[tokio::test]
async fn initiating_viewer_cancellation_does_not_lose_handover_updates() {
    let mut input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = current_time_millis();
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
    let id = session.read().await.proxy_session_id.clone();
    for (port, token) in [(12201, "initiator"), (12202, "remaining")] {
        super::observability::register_hls_cache_stream_for_stats_test(
            &app,
            &session,
            &id,
            token,
            &test_fingerprint_with_addr(test_addr_with_port(port)),
            token,
        )
        .await;
    }
    input.account_disabled = true;
    reload_inputs(&app, vec![Arc::new(input.clone())]);
    let published = Arc::new(tokio::sync::Notify::new());
    let request = {
        let app = app.clone();
        let session = session.clone();
        let id = id.clone();
        let published = published.clone();
        tokio::spawn(async move {
            let _prepared = prepare_hls_origin_runtime(
                &app,
                &session,
                &input,
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                "http://root.example.com/live/root-user/root-pass/12345.m3u8",
                &id,
                &test_fingerprint(),
                ConnectionKind::Normal,
                0,
                HlsOriginWorkKind::Manifest,
                HlsOriginWorkClass::ManifestInteractive,
                now,
            )
            .await
            .unwrap();
            published.notify_one();
            std::future::pending::<()>().await;
        })
    };
    published.notified().await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(app.active_users.panel_streams().await.iter().all(|row| row.provider.as_ref() == "account-a"));
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "late-viewer",
        &test_fingerprint_with_addr(test_addr_with_port(12203)),
        "late-viewer",
    )
    .await;
    let rows = app.active_users.panel_streams().await;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.provider.as_ref() == "account-a"));
}

#[tokio::test]
async fn late_join_and_status_snapshot_use_current_origin_account() {
    use super::super::origin_account_runtime::publish_hls_origin_account_with_status;
    use tuliprox_hls::api::HlsOriginAccountSnapshot;
    let input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, current_time_millis()).await;
    let id = session.read().await.proxy_session_id.clone();
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "existing",
        &test_fingerprint(),
        "existing",
    )
    .await;
    publish_hls_origin_account_with_status(&app, &session, |current| {
        let previous = current.origin_account_binding.as_ref().unwrap();
        let next = HlsOriginAccountBinding::rebound(
            input.name.clone(),
            Arc::from("account-b"),
            previous.session_owner.clone(),
            previous.generation + 1,
            current_time_millis(),
        );
        current.handoff_origin_account(next);
        Ok(HlsOriginAccountSnapshot::from_session(current))
    })
    .await
    .unwrap();
    super::observability::register_hls_cache_stream_for_stats_test(
        &app,
        &session,
        &id,
        "late",
        &test_fingerprint_with_addr(test_addr_with_port(12301)),
        "late",
    )
    .await;
    let rows = app.active_users.panel_streams().await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.provider.as_ref() == "account-b"));
}

#[tokio::test]
async fn handover_waits_for_viewer_projection_without_holding_hls_write_lock() {
    use super::super::origin_account_runtime::publish_hls_origin_account_with_status;
    use tuliprox_hls::api::HlsOriginAccountSnapshot;
    let input = overlap_provider_input();
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, current_time_millis()).await;
    let shared_id = super::super::hls_cache_shared_stream_id(&session.read().await.proxy_session_id);
    let held = app.active_users.lock_shared_stream_provider_projection(shared_id).await;
    let mut publish = Box::pin(publish_hls_origin_account_with_status(&app, &session, |current| {
        Ok(HlsOriginAccountSnapshot::from_session(current))
    }));
    assert!(futures::poll!(publish.as_mut()).is_pending());
    assert!(session.try_write().is_ok(), "manager wait must never retain an HLS lock");
    drop(held);
    publish.await.unwrap();
}
