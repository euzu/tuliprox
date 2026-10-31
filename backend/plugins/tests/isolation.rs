//! Proof that a plugin can never block tokio workers or crash tuliprox: one
//! hostile guest per attack, through the real components, on a two-worker
//! runtime with more guests than workers.
//!
//! Every test is built on `isolation_support::prove` (see there for the four
//! assertions); this file only names the attack, its classification and the
//! data that must come out unchanged. The attacks live in the fixtures as
//! extra modes (`plugin-api/fixtures`), the manifests with the one-second
//! deadline are `probe-evil`, `provider-evil`, `task-probe-spin`, `rpc-spin`,
//! `event-probe-hang`.
//!
//! Not reachable from a Rust guest, so not here: `table.grow` (Rust has no
//! stable table intrinsic; the limit is proven on a hand-written core module in
//! `state.rs`), and a `fetch-playlist` answer past the one million channel cap
//! (it needs a guest of about 260 MiB, the limit is 128 MiB; the guest's own
//! memory limit stops the attack first, and `input_provider.rs` tests the cap).

mod common;
mod isolation_support;

use common::{default_services, items, load_probe, mode, probe_entry, run_stage, snapshot, Probe};
use http::StatusCode;
use isolation_support::{
    assert_recovered, assert_rss_growth_below, healthy_call, healthy_entry, prove, rss_kib, serial, stats, Expect,
    Sleeper, HEALTHY_BOUND, SLEEP_BOUND,
};
use serde_json::{json, Value};
use shared::{
    model::{
        EpgProgramme, EventKind, HookCtx, PluginBreakerStateDto, PluginEpgChannel, PluginHeaderInjector, PluginHost,
        PluginInputRequest, PluginLogQueryDto, PluginOutputRequest, PluginRunReport, PluginUpstreamRequest,
    },
    utils::Internable,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, task::JoinHandle};
use tuliprox_core::model::PluginEntry;
use tuliprox_plugins::{PluginSystem, TaskOutcome, TaskTrigger};

/// A call may take its one-second deadline, the one second of wall-clock slack
/// and, on a busy machine, more.
const CALL_BOUND: Duration = Duration::from_secs(4);
/// The kv flood also pays for committing up to the 50 MiB quota per call when the call ends, in a debug build.
const KV_CALL_BOUND: Duration = Duration::from_secs(30);
/// The coarse bound on host memory growth of an attack, in MiB.
const RSS_BOUND_MIB: u64 = 200;

fn load(mut attackers: Vec<PluginEntry>) -> Option<Probe> {
    attackers.push(healthy_entry());
    load_probe(attackers, default_services)
}

macro_rules! isolation_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            let _serial = serial().await;
            $body.await;
        }
    };
}

// ---------------------------------------------------------------- playlist stage

/// One `after-filter` run over three items; whether the items came back untouched.
async fn stage_call(system: PluginSystem) -> bool {
    let mut list = items(3);
    let before = snapshot(&list);
    run_stage(&system, &mut list).await;
    snapshot(&list) == before
}

async fn prove_stage(attack: &str, settings: Value, error: &str) {
    let Some(probe) = load(vec![probe_entry("probe-evil", settings)]) else { return };
    let before = rss_kib();
    let expect = Expect { plugin: "probe-evil", error: Some(error), breaker_opens: true, max_call: CALL_BOUND };
    prove(&probe.system, &expect, |_| stage_call(probe.system.clone()), |unchanged| {
        assert!(*unchanged, "{attack}: a failed call must leave the items untouched");
    })
    .await;
    assert_rss_growth_below(before, RSS_BOUND_MIB);
}

isolation_test!(an_infinite_loop_in_a_playlist_hook_ends_at_the_deadline, prove_stage("loop", mode("loop"), "deadline exceeded"));

isolation_test!(a_linear_memory_bomb_ends_as_out_of_memory, prove_stage("oom", mode("oom"), "out of memory"));

isolation_test!(unbounded_recursion_ends_as_a_stack_overflow_trap, prove_stage("recurse", mode("recurse"), "trapped"));

isolation_test!(a_log_flood_is_rate_limited_and_ends_at_the_deadline, async {
    let Some(probe) = load(vec![probe_entry("probe-evil", mode("log-flood"))]) else { return };
    let expect = Expect {
        plugin: "probe-evil",
        error: Some("deadline exceeded"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    prove(&probe.system, &expect, |_| stage_call(probe.system.clone()), |unchanged| assert!(*unchanged)).await;

    // Five calls logged as fast as they could: the ring buffer holds its capacity and the rest is counted.
    let kept = probe.system.api_logs("probe-evil", &PluginLogQueryDto::default()).expect("logs").len();
    assert!(kept <= 1000, "the log ring grew to {kept} lines");
    let dropped = stats(&probe.system, "probe-evil").log_dropped;
    assert!(dropped > 1000, "the rate limiter dropped only {dropped} lines");
});

isolation_test!(a_kv_flood_is_refused_at_the_quota_and_ends_at_the_deadline, async {
    let Some(probe) = load(vec![probe_entry("probe-evil", mode("kv-flood"))]) else { return };
    let expect = Expect {
        plugin: "probe-evil",
        error: Some("deadline exceeded"),
        breaker_opens: true,
        max_call: KV_CALL_BOUND,
    };
    prove(&probe.system, &expect, |_| stage_call(probe.system.clone()), |unchanged| assert!(*unchanged)).await;

    let kv = stats(&probe.system, "probe-evil").kv.expect("the plugin has storage");
    assert!(kv.used_bytes > 0, "the flood stored nothing, so the quota was never approached");
    assert!(kv.used_bytes <= kv.quota_bytes, "{} bytes stored, quota {}", kv.used_bytes, kv.quota_bytes);
    let lines = probe.system.api_logs("probe-evil", &PluginLogQueryDto::default()).expect("logs");
    assert!(
        lines.iter().any(|line| line.message.contains("kv-flood refused after") && line.message.contains("quota-exceeded")),
        "the guest never saw quota-exceeded: {lines:?}"
    );
});

// ---------------------------------------------------------------- hanging outbound http

/// A TCP listener that accepts connections and never answers them.
struct Blackhole {
    port: u16,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Blackhole {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the blackhole");
        let port = listener.local_addr().expect("blackhole address").port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(socket);
            }
        });
        Self { port, accepted, task }
    }
}

impl Drop for Blackhole {
    fn drop(&mut self) { self.task.abort(); }
}

isolation_test!(an_outbound_request_that_never_answers_ends_at_the_wall_clock_cap, async {
    let blackhole = Blackhole::start().await;
    let settings = json!({ "mode": "http", "url": format!("http://127.0.0.1:{}/hang", blackhole.port) });
    let Some(probe) = load(vec![probe_entry("probe-evil", settings)]) else { return };
    let expect = Expect {
        plugin: "probe-evil",
        error: Some("deadline exceeded"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    prove(&probe.system, &expect, |_| stage_call(probe.system.clone()), |unchanged| assert!(*unchanged)).await;
    assert!(blackhole.accepted.load(Ordering::SeqCst) >= 1, "the guest never reached the listener");
});

// ---------------------------------------------------------------- epg hooks

async fn programmes_call(system: PluginSystem) -> bool {
    let mut programmes: Vec<EpgProgramme> = (0..5)
        .map(|index| {
            EpgProgramme::new_all(index * 600, (index + 1) * 600, "ch1".intern(), Some("show".intern()), None, None)
        })
        .collect();
    let before: Vec<_> = programmes.iter().map(|p| (p.start, p.stop, p.title.clone())).collect();
    system
        .transform_programmes(&HookCtx::new("").with_input("source"), &mut programmes, &mut PluginRunReport::default())
        .await;
    programmes.iter().map(|p| (p.start, p.stop, p.title.clone())).collect::<Vec<_>>() == before
}

isolation_test!(a_flood_of_programmes_hits_the_host_cap_not_host_memory, async {
    let settings = json!({ "transform_bomb": 20_000 });
    let Some(probe) = load(vec![probe_entry("epg-probe-transform", settings)]) else { return };
    let before = rss_kib();
    let expect = Expect {
        plugin: "epg-probe-transform",
        error: Some("returned 20000 programmes for 5 sent (cap 10000)"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    prove(&probe.system, &expect, |_| programmes_call(probe.system.clone()), |unchanged| assert!(*unchanged)).await;
    assert_rss_growth_below(before, RSS_BOUND_MIB);
});

async fn epg_loaded_call(system: PluginSystem) -> bool {
    let mut channels: Vec<PluginEpgChannel> = (0..3)
        .map(|index| PluginEpgChannel { id: format!("ch{index}"), names: vec![format!("Channel {index}")], icon: None })
        .collect();
    let before = channels.clone();
    system
        .on_epg_loaded(&HookCtx::new("").with_input("source"), &mut channels, &mut PluginRunReport::default())
        .await;
    channels == before
}

isolation_test!(more_channels_than_were_sent_from_on_epg_loaded_hit_the_host_cap, async {
    let settings = json!({ "load_bomb": 5_000 });
    let Some(probe) = load(vec![probe_entry("epg-probe-load", settings)]) else { return };
    let before = rss_kib();
    let expect = Expect {
        plugin: "epg-probe-load",
        error: Some("returned 5000 channels for 3 sent"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    prove(&probe.system, &expect, |_| epg_loaded_call(probe.system.clone()), |unchanged| assert!(*unchanged)).await;
    assert_rss_growth_below(before, RSS_BOUND_MIB);
});

// ---------------------------------------------------------------- providers

async fn fetch_call(system: PluginSystem, plugin: &'static str) -> Result<usize, String> {
    let ctx = HookCtx::new("").with_input("feed");
    let request = PluginInputRequest {
        input_name: "feed",
        url: "http://host/feed",
        username: None,
        password: None,
        options: &[],
    };
    system.fetch_playlist(plugin, &ctx, &request).await.map(|list| list.len())
}

async fn prove_fetch(settings: Value, error: &str) {
    let Some(probe) = load(vec![probe_entry("provider-evil", settings)]) else { return };
    let before = rss_kib();
    let expect = Expect { plugin: "provider-evil", error: Some(error), breaker_opens: true, max_call: CALL_BOUND };
    prove(&probe.system, &expect, |_| fetch_call(probe.system.clone(), "provider-evil"), |outcome| {
        let message = outcome.as_ref().expect_err("an attacked fetch-playlist is an Err, never a list");
        assert!(message.contains(error), "{message}");
    })
    .await;
    assert_rss_growth_below(before, RSS_BOUND_MIB);
}

isolation_test!(an_infinite_loop_in_fetch_playlist_ends_at_the_provider_deadline, prove_fetch(json!({ "mode": "loop" }), "deadline exceeded"));

// The guest's own 32 MiB limit stops this one before the host cap of one million channels (see the module doc).
isolation_test!(a_flood_of_channels_from_fetch_playlist_ends_as_out_of_memory, prove_fetch(json!({ "count": 5_000_000 }), "out of memory"));

isolation_test!(an_oversized_output_chunk_hits_the_host_cap_and_keeps_the_old_file, async {
    let Some(probe) = load(vec![probe_entry("provider-evil", json!({}))]) else { return };
    let dir = tempfile::tempdir().expect("temporary dir");
    let paths: Vec<PathBuf> = (0..4).map(|index| dir.path().join(format!("list-{index}.txt"))).collect();
    for path in &paths {
        std::fs::write(path, "previous run").expect("previous output");
    }
    let before = rss_kib();
    let expect = Expect {
        plugin: "provider-evil",
        error: Some("(cap 16777216)"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    let attack = |index: usize| {
        let (system, path) = (probe.system.clone(), paths[index].clone());
        async move {
            let items = items(2);
            let refs: Vec<_> = items.iter().collect();
            let options = vec![("render_bytes".to_string(), (17 * 1024 * 1024).to_string())];
            let request = PluginOutputRequest { target: "target", format: "", options: &options, path: &path };
            system.render_output("provider-evil", &HookCtx::new("target").with_view("base"), &request, &refs).await
        }
    };
    prove(&probe.system, &expect, attack, |outcome| {
        let message = outcome.as_ref().expect_err("an oversized chunk fails the output");
        assert!(message.contains("(cap 16777216)"), "{message}");
    })
    .await;
    for path in &paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "previous run", "a failed output keeps the last good file");
    }
    assert_rss_growth_below(before, RSS_BOUND_MIB);
});

// ---------------------------------------------------------------- stream headers

static USERS: AtomicUsize = AtomicUsize::new(0);
/// The most headers any call returned.
static MOST_HEADERS: AtomicUsize = AtomicUsize::new(0);

/// A fresh user per call, so the header cache never answers instead of the guest.
async fn headers_call(system: PluginSystem) -> Vec<(String, String)> {
    let user = format!("user-{}", USERS.fetch_add(1, Ordering::SeqCst));
    let request = PluginUpstreamRequest {
        target_id: 1,
        virtual_id: 7,
        url: "http://provider/live/7.ts",
        headers: &[],
        user: &user,
    };
    system.inject_upstream_headers(&request).await
}

isolation_test!(thousands_of_injected_headers_are_cut_to_the_host_cap, async {
    let Some(probe) = load(vec![probe_entry("edge-probe", json!({ "header_count": 3_000 }))]) else { return };
    let expect = Expect { plugin: "edge-probe", error: None, breaker_opens: false, max_call: Duration::from_secs(1) };
    prove(&probe.system, &expect, |_| headers_call(probe.system.clone()), |headers| {
        assert!(headers.len() <= 32, "{} headers got through", headers.len());
        MOST_HEADERS.fetch_max(headers.len(), Ordering::SeqCst);
    })
    .await;
    assert_eq!(MOST_HEADERS.load(Ordering::SeqCst), 32, "no call showed the cap");
});

isolation_test!(a_slow_header_plugin_never_holds_the_stream_past_the_clamp, async {
    let Some(probe) = load(vec![probe_entry("edge-probe", json!({ "delay_ms": 60_000 }))]) else { return };
    // The clamp is 50 ms; the rest is scheduling slack.
    let expect = Expect {
        plugin: "edge-probe",
        error: Some("deadline exceeded"),
        breaker_opens: true,
        max_call: Duration::from_millis(500),
    };
    prove(&probe.system, &expect, |_| headers_call(probe.system.clone()), |headers| {
        assert!(headers.is_empty(), "fail-open: no headers added, got {headers:?}");
    })
    .await;
    assert!(!probe.system.wants_upstream_headers(), "the tripped plugin is no longer asked");
});

// ---------------------------------------------------------------- tasks and rpc

isolation_test!(a_spinning_task_ends_at_its_deadline_and_a_run_is_a_failed_run, async {
    let Some(probe) = load(vec![probe_entry("task-probe-spin", json!({}))]) else { return };
    let expect = Expect {
        plugin: "task-probe-spin",
        error: Some("deadline exceeded"),
        breaker_opens: true,
        max_call: CALL_BOUND,
    };
    let attack = |index: usize| {
        let system = probe.system.clone();
        async move { system.run_task("task-probe-spin", ["spin-a", "spin-b", "spin-c", "spin-d"][index % 4], TaskTrigger::Manual).await }
    };
    prove(&probe.system, &expect, attack, |outcome| {
        let TaskOutcome::Ran(run) = outcome else { panic!("expected a run, got {outcome:?}") };
        assert_eq!(run.summary, None);
        assert!(run.error.as_deref().is_some_and(|error| error.contains("deadline exceeded")), "{:?}", run.error);
    })
    .await;
});

isolation_test!(a_spinning_rpc_method_answers_502_and_never_feeds_the_breaker, async {
    let Some(probe) = load(vec![probe_entry("rpc-spin", json!({}))]) else { return };
    let expect = Expect {
        plugin: "rpc-spin",
        error: Some("deadline exceeded"),
        breaker_opens: false,
        max_call: CALL_BOUND,
    };
    let attack = |_| {
        let system = probe.system.clone();
        async move { system.api_rpc_call("rpc-spin", "spin", b"{}", || false).await }
    };
    prove(&probe.system, &expect, attack, |outcome| {
        let failure = outcome.as_ref().expect_err("a spinning method never answers");
        assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
        assert_eq!(failure.error, "the plugin did not answer in time");
    })
    .await;
});

// ---------------------------------------------------------------- events

isolation_test!(an_event_hook_that_never_returns_cannot_block_emitters_and_a_full_queue_drops, async {
    let Some(probe) = load(vec![probe_entry("event-probe-hang", mode("loop"))]) else { return };
    let system = &probe.system;
    let sleeper = Sleeper::start();

    // The bus subscriber offers a burst: far more than the 256 the queue holds, and the consumer is
    // wedged on the first one for its whole deadline.
    let started = Instant::now();
    for index in 0..1000 {
        system.offer_event(EventKind::PlaylistUpdate, "{}".to_string(), index);
    }
    let offering = started.elapsed();
    assert!(offering < Duration::from_millis(500), "1000 offers took {offering:?}: the emitter waited");
    let dropped = system.dropped_events();
    assert!(dropped >= 700, "only {dropped} of 1000 events were dropped past the 256 slot queue");

    // The consumer works through five events at a deadline each; the breaker then ends the matter.
    let mut slowest_healthy = Duration::ZERO;
    let give_up = Instant::now() + Duration::from_secs(30);
    while stats(system, "event-probe-hang").breaker.state != PluginBreakerStateDto::Open {
        slowest_healthy = slowest_healthy.max(healthy_call(system).await);
        assert!(Instant::now() < give_up, "the breaker never opened: {:?}", stats(system, "event-probe-hang"));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let worst_sleep = sleeper.finish().await;
    assert!(worst_sleep <= SLEEP_BOUND, "a 10 ms sleep took {worst_sleep:?} while the event hook hung");
    assert!(slowest_healthy <= HEALTHY_BOUND, "the healthy plugin took {slowest_healthy:?}");

    let hung = stats(system, "event-probe-hang");
    assert!(hung.failures >= 5, "failures: {}", hung.failures);
    assert!(hung.last_error.as_deref().unwrap_or_default().contains("deadline exceeded"), "{:?}", hung.last_error);
    assert_recovered(system, "event-probe-hang").await;
    healthy_call(system).await;
});
