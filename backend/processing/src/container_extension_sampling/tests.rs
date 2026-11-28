use super::{
    run_sampling, sample_container_extensions, sampling_candidates, ContainerExtensionLedger, ContainerExtensionPortal,
    SamplingAnswer, SamplingLimits, CONTAINER_EXTENSION_LEDGER_FILE,
};
use crate::container_extension_sampling::SamplingReport;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{
    stalker::{StalkerPlaybackMode, StalkerStreamKind},
    stalker_item::StalkerPlaylistItem,
    ConfigInputOptionsDto, ConfigPaths, InputType, StalkerCommandVariantDto, StalkerPlaybackDescriptorDto,
};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{atomic::AtomicUsize, Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{
        AppConfig, Config, ConfigInput, ConfigInputOptions, MaintenanceRegistry, MediaToolCapabilities, SourcesConfig,
        StalkerDeviceProfile, StalkerInputConfig,
    },
    utils::FileLockManager,
};

/// A portal that answers from a script and records which items it was asked about.
struct MockPortal {
    answers: Mutex<VecDeque<SamplingAnswer>>,
    asked: Mutex<Vec<u32>>,
}

impl MockPortal {
    fn answering(answers: impl IntoIterator<Item = SamplingAnswer>) -> Self {
        Self { answers: Mutex::new(answers.into_iter().collect()), asked: Mutex::new(Vec::new()) }
    }

    fn resolved() -> Self { Self::answering(std::iter::repeat_n(SamplingAnswer::Resolved("mkv".into()), 16)) }

    fn asked(&self) -> MutexGuard<'_, Vec<u32>> { self.asked.lock().expect("mock portal lock") }

    fn call_count(&self) -> usize { self.asked().len() }
}

impl ContainerExtensionPortal for MockPortal {
    fn resolve(&self, stream_id: u32) -> impl std::future::Future<Output = SamplingAnswer> + Send {
        self.asked().push(stream_id);
        std::future::ready(self.answers.lock().expect("mock portal lock").pop_front().unwrap_or(SamplingAnswer::Failed))
    }
}

fn limits(max_calls_per_update: u16, min_interval_secs: u16, stop_after_refusals: u8) -> SamplingLimits {
    SamplingLimits { max_calls_per_update, min_interval_secs, stop_after_refusals }
}

fn candidates(stream_ids: impl IntoIterator<Item = u32>) -> Vec<u32> { stream_ids.into_iter().collect() }

fn never_stop() -> impl Fn() -> bool + Sync { || false }

#[tokio::test]
async fn a_run_spends_no_more_than_the_call_budget() {
    let portal = MockPortal::resolved();
    let mut ledger = ContainerExtensionLedger::default();

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates(1..=5), limits(2, 0, 2), &mut ledger, &cancel, None, &never_stop()).await;

    assert_eq!(report.calls, 2, "the budget is an exact call count");
    assert!(report.budget_exhausted);
    assert_eq!(portal.asked().as_slice(), &[1, 2]);
    assert_eq!(report.candidates, 5);
    assert_eq!(portal.call_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn calls_are_paced_by_the_configured_interval() {
    let portal = MockPortal::resolved();
    let mut ledger = ContainerExtensionLedger::default();
    let started = tokio::time::Instant::now();

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates([1, 2, 3]), limits(10, 3, 2), &mut ledger, &cancel, None, &never_stop())
            .await;

    assert_eq!(report.calls, 3);
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(6),
        "three calls wait two intervals; the first call starts immediately"
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_stops_after_the_configured_number_of_consecutive_refusals() {
    let portal = MockPortal::answering([
        SamplingAnswer::Refused,
        SamplingAnswer::Refused,
        SamplingAnswer::Resolved("mkv".into()),
    ]);
    let mut ledger = ContainerExtensionLedger::default();

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates(1..=5), limits(10, 1, 2), &mut ledger, &cancel, None, &never_stop()).await;

    assert_eq!(report.calls, 2);
    assert_eq!(report.refused, 2);
    assert!(report.stopped_after_refusals);
    assert_eq!(portal.call_count(), 2, "no call follows the stop");
}

#[tokio::test(start_paused = true)]
async fn a_success_between_refusals_resets_the_streak() {
    let portal = MockPortal::answering([
        SamplingAnswer::Refused,
        SamplingAnswer::Resolved("mkv".into()),
        SamplingAnswer::Refused,
        SamplingAnswer::Failed,
        SamplingAnswer::Refused,
        SamplingAnswer::Refused,
        SamplingAnswer::Resolved("mp4".into()),
    ]);
    let mut ledger = ContainerExtensionLedger::default();

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates(1..=8), limits(10, 1, 2), &mut ledger, &cancel, None, &never_stop()).await;

    assert_eq!(report.calls, 6, "only the third and fourth refusal of the run form one streak");
    assert_eq!(report.refused, 4);
    assert_eq!(report.failed, 1);
    assert_eq!(report.resolved, 1);
    assert!(report.stopped_after_refusals);
}

#[tokio::test(start_paused = true)]
async fn a_stop_request_ends_the_run_between_two_calls() {
    let portal = MockPortal::resolved();
    let mut ledger = ContainerExtensionLedger::default();
    let calls = AtomicUsize::new(0);
    let should_stop = || calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 1;

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates(1..=5), limits(10, 1, 2), &mut ledger, &cancel, None, &should_stop).await;

    assert_eq!(report.calls, 1, "the stop is honoured before the next call");
    assert!(report.aborted);
    assert!(!report.budget_exhausted);
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_interval_sleep_wakes_and_aborts_immediately() {
    let portal = MockPortal::resolved();
    let mut ledger = ContainerExtensionLedger::default();
    let cancel = CancellationToken::new();
    let cancel_for_stop = cancel.clone();
    let should_stop = move || cancel_for_stop.is_cancelled();

    let started = tokio::time::Instant::now();

    let sampling_task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            run_sampling(&portal, &candidates([1, 2, 3]), limits(10, 60, 2), &mut ledger, &cancel, None, &should_stop)
                .await
        }
    });
    tokio::task::yield_now().await;
    cancel.cancel();

    let report = sampling_task.await.expect("sampling task completes");
    assert_eq!(report.calls, 1, "no second call after cancellation during sleep");
    assert!(report.aborted, "cancellation during sleep must abort the run");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "cancellation must wake the sleep immediately instead of waiting full min_interval_secs (elapsed={:?})",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn stop_requested_during_interval_sleep_prevents_next_portal_call() {
    let portal = MockPortal::resolved();
    let mut ledger = ContainerExtensionLedger::default();
    let stop_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop_requested);
    let should_stop = move || stop_flag.load(std::sync::atomic::Ordering::Acquire);

    let cancel = CancellationToken::new();
    let sampling_task = tokio::spawn(async move {
        run_sampling(&portal, &candidates([1, 2, 3]), limits(10, 5, 2), &mut ledger, &cancel, None, &should_stop).await
    });

    // Advance past the first call into the 5-second inter-call sleep.
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    stop_requested.store(true, std::sync::atomic::Ordering::Release);

    let report = sampling_task.await.expect("sampling task completes");
    assert_eq!(report.calls, 1, "should_stop must be re-checked after waking from sleep before calling resolve");
    assert!(report.aborted);
}

#[tokio::test(start_paused = true)]
async fn pause_gate_is_dropped_during_sleep_and_blocks_next_call_when_written() {
    let portal = Arc::new(MockPortal::resolved());
    let portal_for_task = Arc::clone(&portal);
    let mut ledger = ContainerExtensionLedger::default();
    let cancel = CancellationToken::new();
    let pause_gate = Arc::new(tokio::sync::RwLock::new(()));
    let gate_for_task = Arc::clone(&pause_gate);

    let sampling_task = tokio::spawn(async move {
        run_sampling(
            portal_for_task.as_ref(),
            &candidates([1, 2]),
            limits(10, 60, 2),
            &mut ledger,
            &cancel,
            Some(&gate_for_task),
            &never_stop(),
        )
        .await
    });

    // Let call 1 run and enter the 60s sleep.
    tokio::task::yield_now().await;
    assert_eq!(portal.call_count(), 1);

    // A writer (playlist update) must acquire the gate immediately during the 60s sleep.
    let write_guard =
        pause_gate.clone().try_write_owned().expect("pause_gate write lock must not be held across interval sleep");

    // Advance past the 60s sleep: call 2 must wait while the write guard is held.
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(portal.call_count(), 1, "call 2 must wait while playlist update holds pause_gate");

    drop(write_guard);
    let report = sampling_task.await.expect("sampling task completes");
    assert_eq!(report.calls, 2);
}

#[tokio::test]
async fn only_items_without_a_provider_extension_and_without_a_ledger_entry_are_sampled() {
    let mut with_provider_value = stalker_item(1, "ffmpeg http://portal/movie/1");
    with_provider_value.container_extension = Some("mkv".into());
    let mut with_placeholder = stalker_item(2, "ffmpeg http://portal/movie/2");
    with_placeholder.container_extension = Some("null".into());
    let mut with_url_extension = stalker_item(3, "ffmpeg http://portal/movie/3");
    with_url_extension.stream_url = "http://cdn.example/movie/3.mp4".into();
    let live =
        StalkerPlaylistItem { stream_kind: StalkerStreamKind::Live, ..stalker_item(4, "ffmpeg http://portal/live/4") };
    let without_cmd = StalkerPlaylistItem { cmd: "  ".into(), ..stalker_item(5, "  ") };
    let plain = stalker_item(6, "ffmpeg http://portal/movie/6");
    let items = [with_provider_value, with_placeholder, with_url_extension, live, without_cmd, plain];

    let mut ledger = ContainerExtensionLedger::default();
    ledger.insert(2, "avi");

    let candidates = sampling_candidates(&items, &ledger);

    assert_eq!(
        candidates,
        vec![6],
        "a provider value excludes an item even when it is a placeholder, a ledger entry excludes it, \
         a URL extension makes the answer unused, only movies are sampled, and an item without a command is skipped"
    );
}

#[tokio::test]
async fn an_item_with_only_a_descriptor_candidate_is_sampled() {
    let item = StalkerPlaylistItem {
        playback_descriptor: Some(StalkerPlaybackDescriptorDto {
            primary_mode: StalkerPlaybackMode::TempLinkNginx,
            candidates: vec![StalkerCommandVariantDto {
                cmd: "ffmpeg http://portal/descriptor/9".to_string(),
                playback_mode: StalkerPlaybackMode::TempLinkNginx,
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..stalker_item(9, "ffmpeg http://portal/raw/9")
    };

    assert_eq!(
        sampling_candidates(&[item], &ContainerExtensionLedger::default()),
        vec![9],
        "a descriptor candidate is enough for a call; the command itself comes from the catalog read"
    );
}

fn stalker_item(stream_id: u32, cmd: &str) -> StalkerPlaylistItem {
    StalkerPlaylistItem {
        stream_id,
        cmd: cmd.into(),
        stream_kind: StalkerStreamKind::Movie,
        ..StalkerPlaylistItem::default()
    }
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tuliprox-sampling-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[tokio::test]
async fn the_ledger_round_trips_and_replaces_its_file_atomically() {
    let dir = scratch_dir("ledger");
    let path = dir.join(CONTAINER_EXTENSION_LEDGER_FILE);

    let mut ledger = ContainerExtensionLedger::default();
    ledger.insert(7, "MKV");
    ledger.insert(8, "null");
    ledger.store(&path).await.expect("ledger is written");
    assert!(!dir.join("container_extension_ledger.json.tmp").exists(), "no temporary file is left behind");

    let loaded = ContainerExtensionLedger::load(&path).await.expect("ledger is read back");
    assert_eq!(loaded.get(7), Some("mkv"));
    assert_eq!(loaded.get(8), None, "a placeholder never reaches the file");
    assert_eq!(loaded.len(), 1);

    // A second write over the existing file keeps it readable, never half-written.
    let mut second = loaded;
    second.insert(9, "avi");
    second.store(&path).await.expect("ledger is replaced");
    let loaded = ContainerExtensionLedger::load(&path).await.expect("replaced ledger is read back");
    assert_eq!(loaded.iter().collect::<Vec<_>>(), vec![(7, "mkv"), (9, "avi")]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn entries_of_items_that_no_longer_exist_are_pruned() {
    let dir = scratch_dir("prune");
    let path = dir.join(CONTAINER_EXTENSION_LEDGER_FILE);
    let mut ledger = ContainerExtensionLedger::default();
    ledger.insert(1, "mkv");
    ledger.insert(2, "avi");
    ledger.insert(3, "mp4");
    ledger.store(&path).await.expect("ledger is written");

    let mut loaded = ContainerExtensionLedger::load(&path).await.expect("ledger is read back");
    let existing = std::collections::HashSet::from([2, 3, 4]);
    assert_eq!(loaded.prune_to(&existing), 1);
    loaded.store(&path).await.expect("pruned ledger is written");

    let loaded = ContainerExtensionLedger::load(&path).await.expect("pruned ledger is read back");
    assert_eq!(loaded.iter().collect::<Vec<_>>(), vec![(2, "avi"), (3, "mp4")]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_ledger_is_empty_and_a_damaged_one_is_reported() {
    let dir = scratch_dir("damaged");
    let path = dir.join(CONTAINER_EXTENSION_LEDGER_FILE);

    assert!(ContainerExtensionLedger::load(&path).await.expect("a missing ledger is empty").is_empty());

    std::fs::write(&path, b"{ not json").expect("damaged ledger");
    assert!(ContainerExtensionLedger::load(&path).await.is_err(), "a damaged ledger is reported, not silently used");

    let _ = std::fs::remove_dir_all(&dir);
}

fn sampling_input(input_type: InputType, enabled: bool, url: &str) -> ConfigInput {
    let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
        container_extension_sampling: shared::model::ContainerExtensionSamplingDto {
            enabled,
            ..shared::model::ContainerExtensionSamplingDto::default()
        },
        ..ConfigInputOptionsDto::default()
    });
    ConfigInput {
        id: 1,
        name: "portal".into(),
        input_type,
        url: url.to_string(),
        enabled: true,
        options: Some(options),
        stalker: Some(StalkerInputConfig {
            device: Some(StalkerDeviceProfile {
                mac_address: Some("00:1A:79:00:00:0A".to_string()),
                ..Default::default()
            }),
            ..StalkerInputConfig::default()
        }),
        ..ConfigInput::default()
    }
}

fn test_app_config(config_path: &Path) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        plugins: Arc::default(),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: config_path.display().to_string(),
            config_path: config_path.display().to_string(),
            storage_path: config_path.display().to_string(),
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
        failover: Arc::default(),
    })
}

#[tokio::test]
async fn a_disabled_input_never_starts_a_sampling_run() {
    let dir = scratch_dir("disabled");
    let app_config = test_app_config(&dir);
    let http = reqwest::Client::new();
    let maintenance = MaintenanceRegistry::in_memory();
    let cancel = CancellationToken::new();
    let input = sampling_input(InputType::Stalker, false, "http://portal.example/stalker_portal/c/");

    assert!(SamplingLimits::from_input(&input).is_none(), "the disabled block exposes no limits");

    let report = sample_container_extensions(&app_config, &http, &input, &maintenance, &cancel, None)
        .await
        .expect("a disabled input is not an error");
    assert!(report.is_none(), "no run happens, so no portal request is made");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_non_stalker_input_never_samples_even_with_the_option_enabled() {
    let dir = scratch_dir("non-stalker");
    let app_config = test_app_config(&dir);
    let http = reqwest::Client::new();
    let maintenance = MaintenanceRegistry::in_memory();
    let cancel = CancellationToken::new();
    let input = sampling_input(InputType::Xtream, true, "http://portal.example");

    assert!(SamplingLimits::from_input(&input).is_some(), "the option is configured");
    let report = sample_container_extensions(&app_config, &http, &input, &maintenance, &cancel, None)
        .await
        .expect("a non-Stalker input is not an error");
    assert!(report.is_none(), "only a Stalker input has a create_link path to sample");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_input_in_maintenance_never_starts_a_sampling_run() {
    let dir = scratch_dir("maintenance");
    let app_config = test_app_config(&dir);
    let http = reqwest::Client::new();
    let maintenance = MaintenanceRegistry::in_memory();
    maintenance.add("portal", Some("test".to_string()), 0).await.expect("maintenance entry");
    let cancel = CancellationToken::new();
    let input = sampling_input(InputType::Stalker, true, "http://portal.example/stalker_portal/c/");

    let report = sample_container_extensions(&app_config, &http, &input, &maintenance, &cancel, None)
        .await
        .expect("maintenance is not an error");
    assert!(report.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A report's counters must add up to what the portal answered.
#[tokio::test(start_paused = true)]
async fn the_report_counts_every_answer_exactly_once() {
    let portal = MockPortal::answering([
        SamplingAnswer::Resolved("mkv".into()),
        SamplingAnswer::Failed,
        SamplingAnswer::Refused,
    ]);
    let mut ledger = ContainerExtensionLedger::default();

    let cancel = CancellationToken::new();
    let report =
        run_sampling(&portal, &candidates([1, 2, 3]), limits(10, 1, 5), &mut ledger, &cancel, None, &never_stop())
            .await;

    assert_eq!(report.calls, report.resolved + report.refused + report.failed);
    assert_eq!(
        report,
        SamplingReport { candidates: 3, calls: 3, resolved: 1, refused: 1, failed: 1, ..SamplingReport::default() }
    );
    assert_eq!(ledger.get(1), Some("mkv"));
}
