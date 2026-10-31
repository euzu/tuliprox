//! Discovery, approval pins, lifecycle, the circuit breaker and hot reload
//! (plan §7 *Registry lifecycle*, §9 *Install integrity*, §18 *W3 runtime freeze*).
//!
//! Every directory `<plugins dir>/<id>/` with a `manifest.yml` becomes one
//! [`PluginRecord`], whatever its state: a broken manifest is `Invalid` and an
//! unapproved plugin is `PendingApproval`, and neither is compiled. A plugin
//! whose wasm hash and capabilities match its pin in
//! `<data_dir>/approvals.json` (or whose `plugins.plugins[]` entry is `enabled`
//! the first time it is seen) is compiled, linked and introspected, and its
//! exports are checked against the manifest's `hooks:`.
//!
//! The registry is a swapped [`RegistrySnapshot`]: a reload, from the config or
//! from the directory watcher, builds a whole new one and replaces the old one
//! atomically. Everything here is synchronous: loads and reloads compile
//! components on the calling thread, which must be a blocking one.

use crate::{
    manifest::{hook::HookId, package::check_package, Capabilities, FailMode, Manifest},
    runtime::{CallError, PluginLimits, PreparedPlugin, Runtime},
    state::{PluginEnv, Shared},
};
use arc_swap::ArcSwap;
use notify::{
    event::{AccessKind, AccessMode, ModifyKind},
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use shared::{
    error::TuliproxError,
    model::{
        PluginFingerprintStats, PluginHook, PluginHookStats, PluginScope, DEFAULT_PLUGINS_BATCH_SIZE,
        DEFAULT_PLUGIN_PRIORITY,
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, MutexGuard, PoisonError, Weak,
    },
    time::Duration,
};
use tuliprox_core::{
    model::{PluginEntry, PluginsConfig},
    utils::{
        tmp_path_for, CallOutcome, CircuitBreaker, CircuitBreakerPolicy, CircuitSnapshot, CircuitState, SystemClock,
    },
};
use tuliprox_plugin_api::{
    bindings::exports::tuliprox::plugin::{
        chno_hooks, dedupe_hooks, epg_enrich_hooks, epg_load_hooks, epg_match_hooks, epg_transform_hooks, event_hooks,
        filter_hooks, group_merge_hooks, input_provider, lifecycle, metadata_hooks, metrics_hooks, notification_hooks,
        output_generator, playlist_hooks, rpc, sort_hooks, sort_key_hooks, source_hooks, stream_hooks, tasks,
    },
    ABI_VERSION, EXPORT_INTERFACES,
};
use wasmtime::{component::Component, Engine};

pub(crate) const MANIFEST_FILE: &str = "manifest.yml";
pub(crate) const WASM_FILE: &str = "plugin.wasm";
/// The approval pins, inside the shared data dir.
const APPROVALS_FILE: &str = "approvals.json";
/// Consecutive counted failures that trip a plugin's circuit breaker.
const BREAKER_THRESHOLD: u32 = 5;
/// How long the plugins directory must stay quiet before it is re-scanned.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

/// Where a plugin is in its lifecycle (plan §7 state diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginState {
    Discovered,
    /// Manifest or wasm validation failed; see the failure reason.
    Invalid,
    /// Valid, but not approved, or its wasm hash or capabilities changed since.
    PendingApproval,
    /// Approved but not yet compiled. Never stored by the W3 registry, which
    /// loads in the same scan; `PUT …/approve` (Phase 5.3) reports it.
    #[allow(dead_code, reason = "part of the frozen lifecycle; first stored by Phase 5.3 approval")]
    Approved,
    /// Compiled and linked, not enabled.
    Loaded,
    Enabled,
    Disabled,
    /// The circuit breaker tripped; skipped until re-enabled or reloaded.
    Failed,
}

impl PluginState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Invalid => "invalid",
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Loaded => "loaded",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
        }
    }
}

/// What a plugin contributed to a run (plan §7 *Determinism*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginFingerprint {
    pub(crate) id: Arc<str>,
    pub(crate) version: String,
    pub(crate) wasm_sha256: String,
    /// sha256 over the settings JSON **after** `${env:…}` resolution.
    pub(crate) settings_hash: String,
    pub(crate) abi: &'static str,
    /// sha256 over all of the above: what the processing report carries, so
    /// a changed setting shows without the settings hash leaving the host.
    pub(crate) digest: String,
}

impl PluginFingerprint {
    fn new(id: &Arc<str>, version: String, wasm_sha256: String, settings_json: &str) -> Self {
        let settings_hash = sha256_hex(settings_json.as_bytes());
        let digest = sha256_hex(format!("{id}\n{version}\n{wasm_sha256}\n{settings_hash}\n{ABI_VERSION}").as_bytes());
        Self { id: Arc::clone(id), version, wasm_sha256, settings_hash, abi: ABI_VERSION, digest }
    }

    /// The fingerprint at `kv_generation`: the load-time digest at 0, else a
    /// digest over it and the generation, so task-written KV shows as a
    /// plugin change in playlist watch.
    pub(crate) fn at_generation(&self, kv_generation: u64) -> Self {
        if kv_generation == 0 {
            return self.clone();
        }
        let digest = sha256_hex(format!("{}\nkv:{kv_generation}", self.digest).as_bytes());
        Self { digest, ..self.clone() }
    }

    pub(crate) fn stats(&self) -> PluginFingerprintStats {
        PluginFingerprintStats {
            id: self.id.to_string(),
            version: self.version.clone(),
            wasm_sha256: self.wasm_sha256.clone(),
            abi: self.abi.to_string(),
            fingerprint: self.digest.clone(),
        }
    }
}

/// A plugin that compiled, linked and passed the export check.
pub(crate) struct LoadedPlugin {
    pub(crate) prepared: PreparedPlugin,
    pub(crate) env: Arc<PluginEnv>,
    pub(crate) lifecycle: lifecycle::GuestIndices,
    /// `Some` when the component exports `playlist-hooks`.
    pub(crate) playlist_hooks: Option<playlist_hooks::GuestIndices>,
    /// `Some` when the component exports `source-hooks`.
    pub(crate) source_hooks: Option<source_hooks::GuestIndices>,
    /// The W5 interfaces, each `Some` when the component exports it.
    pub(crate) sort_hooks: Option<sort_hooks::GuestIndices>,
    pub(crate) sort_key_hooks: Option<sort_key_hooks::GuestIndices>,
    pub(crate) chno_hooks: Option<chno_hooks::GuestIndices>,
    pub(crate) dedupe_hooks: Option<dedupe_hooks::GuestIndices>,
    pub(crate) group_merge_hooks: Option<group_merge_hooks::GuestIndices>,
    pub(crate) filter_hooks: Option<filter_hooks::GuestIndices>,
    pub(crate) epg_load_hooks: Option<epg_load_hooks::GuestIndices>,
    pub(crate) epg_transform_hooks: Option<epg_transform_hooks::GuestIndices>,
    pub(crate) epg_enrich_hooks: Option<epg_enrich_hooks::GuestIndices>,
    pub(crate) epg_match_hooks: Option<epg_match_hooks::GuestIndices>,
    pub(crate) metadata_hooks: Option<metadata_hooks::GuestIndices>,
    /// The W6a interfaces (plan §6 *Hooks outside the per-target pipeline*),
    /// each `Some` when the component exports it.
    pub(crate) event_hooks: Option<event_hooks::GuestIndices>,
    pub(crate) metrics_hooks: Option<metrics_hooks::GuestIndices>,
    pub(crate) notification_hooks: Option<notification_hooks::GuestIndices>,
    pub(crate) input_provider: Option<input_provider::GuestIndices>,
    pub(crate) output_generator: Option<output_generator::GuestIndices>,
    pub(crate) stream_hooks: Option<stream_hooks::GuestIndices>,
    pub(crate) tasks: Option<tasks::GuestIndices>,
    /// `Some` when the component exports `rpc` (W6b, `ui.rpc_exports`).
    pub(crate) rpc: Option<rpc::GuestIndices>,
    /// The manifest hooks the host can run today.
    pub(crate) hooks: Vec<PluginHook>,
    /// The manifest `scope:`, compiled once here.
    pub(crate) scope: Arc<PluginScope>,
    pub(crate) fail_mode: FailMode,
    pub(crate) fingerprint: PluginFingerprint,
}

impl LoadedPlugin {
    /// The fingerprint now, with the current `kv_generation`.
    pub(crate) fn current_fingerprint(&self) -> PluginFingerprint {
        self.fingerprint.at_generation(self.env.kv_generation.load(Ordering::Relaxed))
    }
}

/// Per-plugin call counters. `calls` and `failures` cover completed hook
/// invocations; a [`CallError::Budget`] is a host condition and is counted
/// apart, in `budget_skips`.
#[derive(Default)]
struct Counters {
    calls: AtomicU64,
    failures: AtomicU64,
    budget_skips: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// Per-hook totals over every target since this record was loaded.
    hooks: Mutex<Vec<PluginHookStats>>,
}

/// What the management API shows of a plugin's runtime health.
#[derive(Debug, Clone)]
pub(crate) struct PluginStats {
    /// Hook invocations that ran to an outcome.
    pub(crate) calls: u64,
    /// Of those, the ones that trapped, timed out or returned `err`.
    pub(crate) failures: u64,
    /// Invocations the host could not start for lack of memory budget or an
    /// instance slot. Not the plugin's fault, so not in `failures`.
    pub(crate) budget_skips: u64,
    pub(crate) last_error: Option<String>,
    pub(crate) breaker: CircuitSnapshot,
}

/// One plugin directory, whatever its state.
pub(crate) struct PluginRecord {
    pub(crate) id: Arc<str>,
    pub(crate) dir: PathBuf,
    /// From `plugins.plugins[].priority`; lower runs first.
    pub(crate) priority: i32,
    /// `None` when the manifest did not parse.
    pub(crate) manifest: Option<Arc<Manifest>>,
    pub(crate) wasm_sha256: Option<String>,
    /// Fully qualified export interfaces the component implements.
    pub(crate) exports: Vec<String>,
    pub(crate) loaded: Option<LoadedPlugin>,
    state: PluginState,
    failure: Option<String>,
    breaker: CircuitBreaker,
    counters: Counters,
}

impl PluginRecord {
    pub(crate) fn new(id: &str, dir: PathBuf, priority: i32) -> Self {
        // Stays open until `reset_breaker` or a reload replaces the record: the
        // trip is followed by `force_open`, which has no cool-down.
        let policy = CircuitBreakerPolicy { failure_threshold: BREAKER_THRESHOLD, ..CircuitBreakerPolicy::default() };
        Self {
            id: Arc::from(id),
            dir,
            priority,
            manifest: None,
            wasm_sha256: None,
            exports: Vec::new(),
            loaded: None,
            state: PluginState::Discovered,
            failure: None,
            breaker: CircuitBreaker::new(policy, SystemClock).expect("the plugin breaker policy is valid"),
            counters: Counters::default(),
        }
    }

    fn tripped(&self) -> bool { matches!(self.breaker.snapshot().state, CircuitState::Open | CircuitState::HalfOpen) }

    /// The effective state: [`PluginState::Failed`] once the breaker trips.
    pub(crate) fn state(&self) -> PluginState {
        if self.state == PluginState::Enabled && self.tripped() {
            PluginState::Failed
        } else {
            self.state
        }
    }

    /// Why the plugin is `Invalid`, `PendingApproval` or `Failed`.
    pub(crate) fn failure(&self) -> Option<String> {
        if self.state() == PluginState::Failed {
            let last = lock(&self.counters.last_error).clone().unwrap_or_else(|| "unknown".to_string());
            return Some(format!(
                "circuit breaker open after {BREAKER_THRESHOLD} consecutive failures; last error: {last}"
            ));
        }
        self.failure.clone()
    }

    /// Count one hook call's outcome against the plugin's circuit breaker.
    ///
    /// [`CallError::Budget`] is a host condition (no memory budget, no free
    /// instance slot) and never counts. Any other error counts; the
    /// [`BREAKER_THRESHOLD`]th in a row trips the breaker and the plugin reads
    /// as [`PluginState::Failed`] until [`PluginRecord::reset_breaker`] or a
    /// reload.
    pub(crate) fn report(&self, outcome: Result<(), &CallError>) {
        let call_outcome = match outcome {
            Ok(()) => CallOutcome::Success,
            Err(CallError::Budget { .. }) => {
                self.counters.budget_skips.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(CallError::Deadline | CallError::OutOfMemory) => CallOutcome::Transient,
            Err(CallError::Trap(_) | CallError::Instantiate(_)) => CallOutcome::Permanent,
        };
        self.counters.calls.fetch_add(1, Ordering::Relaxed);
        if let Err(err) = outcome {
            self.counters.failures.fetch_add(1, Ordering::Relaxed);
            *lock(&self.counters.last_error) = Some(err.to_string());
        }
        // A rejection means the breaker is already open: a call that was in
        // flight when it tripped. Its outcome is already counted above.
        let Ok(permit) = self.breaker.acquire() else {
            return;
        };
        let _ = permit.report(call_outcome);
        let snapshot = self.breaker.snapshot();
        if snapshot.state == CircuitState::Open && !snapshot.forced_open {
            self.breaker.force_open();
            log::error!(
                "[plugin:{}] {BREAKER_THRESHOLD} consecutive failures, disabled until re-enabled or reloaded: {}",
                self.id,
                lock(&self.counters.last_error).as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Close the circuit breaker again (a manual re-enable). The counters stay.
    #[allow(dead_code, reason = "called by `PUT …/enable` (Phase 5.3)")]
    pub(crate) fn reset_breaker(&self) { self.breaker.reset(); }

    /// Call counters, the last error and the breaker, for the management API.
    pub(crate) fn stats(&self) -> PluginStats {
        PluginStats {
            calls: self.counters.calls.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            budget_skips: self.counters.budget_skips.load(Ordering::Relaxed),
            last_error: lock(&self.counters.last_error).clone(),
            breaker: self.breaker.snapshot(),
        }
    }

    /// Add one hook run to the per-hook totals the management API shows.
    pub(crate) fn record_hook(&self, stats: &PluginHookStats) {
        let mut hooks = lock(&self.counters.hooks);
        match hooks.iter_mut().find(|entry| entry.hook == stats.hook) {
            Some(entry) => {
                entry.kept += stats.kept;
                entry.dropped += stats.dropped;
                entry.modified += stats.modified;
                entry.errors += stats.errors;
                entry.ignored_changes += stats.ignored_changes;
                entry.duration_ms += stats.duration_ms;
            }
            None => hooks.push(stats.clone()),
        }
    }

    /// The per-hook totals since load (a reload starts them over).
    pub(crate) fn hook_totals(&self) -> Vec<PluginHookStats> { lock(&self.counters.hooks).clone() }

    /// Enabled, not failed, loaded, and its manifest lists `hook`.
    pub(crate) fn runs(&self, hook: PluginHook) -> bool {
        self.state() == PluginState::Enabled && self.loaded.as_ref().is_some_and(|loaded| loaded.hooks.contains(&hook))
    }
}

/// An immutable view of every plugin, swapped whole on reload.
pub(crate) struct RegistrySnapshot {
    /// Sorted by (priority, id): the run order.
    pub(crate) plugins: Vec<Arc<PluginRecord>>,
    /// `plugins.batch_size` of the scan: items per hook call.
    pub(crate) batch_size: usize,
}

impl Default for RegistrySnapshot {
    fn default() -> Self { Self { plugins: Vec::new(), batch_size: DEFAULT_PLUGINS_BATCH_SIZE as usize } }
}

impl RegistrySnapshot {
    pub(crate) fn get(&self, id: &str) -> Option<&Arc<PluginRecord>> {
        self.plugins.iter().find(|plugin| &*plugin.id == id)
    }

    /// The plugins that run `hook`, in run order.
    pub(crate) fn runnable(&self, hook: PluginHook) -> impl Iterator<Item = &Arc<PluginRecord>> {
        self.plugins.iter().filter(move |plugin| plugin.runs(hook))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> { mutex.lock().unwrap_or_else(PoisonError::into_inner) }

// ---------------------------------------------------------------------------
// Approval pins
// ---------------------------------------------------------------------------

/// What the admin approved: the exact wasm and the capabilities it asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApprovalPin {
    pub(crate) wasm_sha256: String,
    /// Capability names (`network`, `storage`, ...), sorted.
    pub(crate) capabilities: Vec<String>,
    /// MCP switch 2 (plan §7): the admin approved offering the `mcp: true`
    /// exports as MCP tools. Only `PUT …/approve` sets it; a trust-on-first-use
    /// pin never does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) mcp: bool,
}

/// `<data_dir>/approvals.json`: `{"<plugin id>": {"wasm_sha256": "...", "capabilities": ["network"]}}`.
pub(crate) type Approvals = BTreeMap<String, ApprovalPin>;

pub(crate) fn approvals_path(data_dir: &Path) -> PathBuf { data_dir.join(APPROVALS_FILE) }

/// The pins on disk. A missing file means none; a corrupt one is logged and
/// treated as none, so nothing stays approved on the strength of a file that
/// cannot be read.
pub(crate) fn read_approvals(path: &Path) -> Approvals {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Approvals::new(),
        Err(err) => {
            log::error!("cannot read the plugin approvals {}: {err}", path.display());
            return Approvals::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        log::error!("ignoring the corrupt plugin approvals {}: {err}", path.display());
        Approvals::new()
    })
}

/// Write the pins atomically: a temp file next to the target, synced, then
/// renamed over it.
pub(crate) fn write_approvals(path: &Path, approvals: &Approvals) -> io::Result<()> {
    let content = serde_json::to_vec_pretty(approvals).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&content)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// The capability names a manifest asks for, sorted.
pub(crate) fn capability_names(capabilities: Capabilities) -> Vec<String> {
    let mut names: Vec<String> =
        capabilities.granted().into_iter().map(|capability| capability.as_str().to_string()).collect();
    names.sort_unstable();
    names
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The pin matches.
    Approved,
    /// No pin, but the config entry is `enabled`: pin what is on disk.
    PinNow,
    Pending(String),
}

fn short(sha256: &str) -> &str { sha256.get(..12).unwrap_or(sha256) }

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Plan §18 *Approval without a PUT route*: no pin and an enabled config entry
/// approves (trust on first use); no pin otherwise waits; a pin that no longer
/// matches the hash or the capabilities always waits.
fn decide_approval(
    pin: Option<&ApprovalPin>,
    wasm_sha256: &str,
    capabilities: &[String],
    entry_enabled: bool,
) -> Verdict {
    let Some(pin) = pin else {
        return if entry_enabled {
            Verdict::PinNow
        } else {
            Verdict::Pending(format!("not approved yet; it asks for capabilities: {}", list(capabilities)))
        };
    };
    let mut changes = Vec::new();
    if pin.wasm_sha256 != wasm_sha256 {
        changes.push(format!(
            "{WASM_FILE} changed since it was approved (approved sha256 {}..., now {}...)",
            short(&pin.wasm_sha256),
            short(wasm_sha256)
        ));
    }
    let mut approved = pin.capabilities.clone();
    approved.sort_unstable();
    if approved.as_slice() != capabilities {
        changes.push(format!(
            "capabilities changed since it was approved (approved: {}; now: {})",
            list(&approved),
            list(capabilities)
        ));
    }
    if changes.is_empty() {
        Verdict::Approved
    } else {
        Verdict::Pending(changes.join("; "))
    }
}

/// Enabled, disabled or merely loaded, from the operator's config entry.
fn state_for_entry(entry: Option<&PluginEntry>) -> PluginState {
    match entry {
        Some(entry) if entry.enabled => PluginState::Enabled,
        Some(_) => PluginState::Disabled,
        None => PluginState::Loaded,
    }
}

// ---------------------------------------------------------------------------
// Exports and hooks
// ---------------------------------------------------------------------------

/// The fully qualified export name of the interface `name` (`playlist-hooks`).
pub(crate) fn interface_name(name: &str) -> String { format!("tuliprox:plugin/{name}@{ABI_VERSION}") }

/// Split a component's export names into the interfaces the host knows
/// (in [`EXPORT_INTERFACES`] order) and the others.
fn split_exports<'a>(names: impl IntoIterator<Item = &'a str>) -> (Vec<String>, Vec<String>) {
    let names: Vec<&str> = names.into_iter().collect();
    let known =
        EXPORT_INTERFACES.iter().filter(|known| names.contains(known)).map(|known| (*known).to_string()).collect();
    let unknown = names.into_iter().filter(|name| !EXPORT_INTERFACES.contains(name)).map(ToString::to_string).collect();
    (known, unknown)
}

/// `lifecycle` is mandatory, every listed hook needs its interface, and every
/// exported hook interface needs a hook that uses it (plan §5 *How the host
/// loads*). Reports every problem at once.
pub(crate) fn check_hooks(hooks: &[HookId], exports: &[String]) -> Result<(), String> {
    let lifecycle = interface_name("lifecycle");
    let mut problems = Vec::new();
    if !exports.contains(&lifecycle) {
        problems.push(format!("the component does not export the mandatory `{lifecycle}`"));
    }
    for hook in hooks {
        let wanted = interface_name(hook.wit_interface());
        if !exports.contains(&wanted) {
            problems.push(format!(
                "hook `{}` is listed in {MANIFEST_FILE} but the component does not export `{wanted}`",
                hook.as_str()
            ));
        }
    }
    for export in exports.iter().filter(|export| **export != lifecycle) {
        if !hooks.iter().any(|hook| interface_name(hook.wit_interface()) == *export) {
            problems.push(format!("the component exports `{export}` but no hook in {MANIFEST_FILE} uses it"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

// ---------------------------------------------------------------------------
// Settings and secrets
// ---------------------------------------------------------------------------

/// One step of a secret path (`servers[].key`, `extra.*`).
#[derive(Debug, PartialEq, Eq)]
enum Step<'a> {
    Key(&'a str),
    /// `*`: every value of an object (`additionalProperties`).
    Any,
    /// `[]`: every element of an array.
    Items,
}

fn parse_secret_path(path: &str) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    if path == "(root)" {
        return steps;
    }
    for segment in path.split('.') {
        let mut name = segment;
        let mut items = 0;
        while let Some(rest) = name.strip_suffix("[]") {
            name = rest;
            items += 1;
        }
        if name == "*" {
            steps.push(Step::Any);
        } else if !name.is_empty() {
            steps.push(Step::Key(name));
        }
        steps.extend(std::iter::repeat_with(|| Step::Items).take(items));
    }
    steps
}

fn collect_secrets(value: &Value, steps: &[Step<'_>], out: &mut Vec<String>) {
    let Some((step, rest)) = steps.split_first() else {
        if let Value::String(secret) = value {
            if !secret.is_empty() && !out.contains(secret) {
                out.push(secret.clone());
            }
        }
        return;
    };
    match (step, value) {
        (Step::Key(name), Value::Object(map)) => {
            if let Some(child) = map.get(*name) {
                collect_secrets(child, rest, out);
            }
        }
        (Step::Any, Value::Object(map)) => {
            for child in map.values() {
                collect_secrets(child, rest, out);
            }
        }
        (Step::Items, Value::Array(items)) => {
            for item in items {
                collect_secrets(item, rest, out);
            }
        }
        _ => {}
    }
}

/// The resolved string values found at the schema's secret `paths`.
fn secret_values(settings: &Value, paths: &[String]) -> Vec<String> {
    let mut secrets = Vec::new();
    for path in paths {
        collect_secrets(settings, &parse_secret_path(path), &mut secrets);
    }
    secrets
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

// ---------------------------------------------------------------------------
// Scanning and loading
// ---------------------------------------------------------------------------

struct Candidate {
    id: String,
    dir: PathBuf,
}

/// Every `<directory>/<id>/` holding a manifest, by id. Loose files, hidden
/// directories and directories without a manifest are skipped.
fn discover(directory: &Path) -> io::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let dir = entry.path();
        let Some(id) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        if id.starts_with('.') || !dir.is_dir() || !dir.join(MANIFEST_FILE).is_file() {
            continue;
        }
        candidates.push(Candidate { id, dir });
    }
    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(candidates)
}

/// Why a plugin stopped before it was loaded.
struct Rejection {
    state: PluginState,
    reason: String,
}

impl Rejection {
    fn invalid(reason: impl Into<String>) -> Self { Self { state: PluginState::Invalid, reason: reason.into() } }
}

/// Everything one scan reads.
struct ScanContext<'a> {
    config: &'a PluginsConfig,
    runtime: &'a Runtime,
    shared: &'a Shared,
    approvals: &'a Approvals,
}

pub(crate) fn export_names(engine: &Engine, component: &Component) -> (Vec<String>, Vec<String>) {
    let ty = component.component_type();
    split_exports(ty.exports(engine).map(|(name, _)| name))
}

/// Compile, link and introspect an approved plugin and fill in `record.loaded`.
fn load(
    ctx: &ScanContext<'_>,
    record: &mut PluginRecord,
    manifest: &Manifest,
    wasm: &[u8],
    wasm_sha256: &str,
    secret_paths: &[String],
    entry: Option<&PluginEntry>,
) -> Result<(), Rejection> {
    let limits = PluginLimits::clamp(&manifest.resources, ctx.runtime.config());
    let (component, prepared) =
        ctx.runtime.prepare_cached(wasm_sha256, wasm, limits).map_err(|err| Rejection::invalid(err.message()))?;

    let (exports, unknown) = export_names(ctx.runtime.engine(), &component);
    for name in &unknown {
        log::warn!("plugin `{}` exports `{name}`, which this host does not know; ignored", record.id);
    }
    record.exports = exports;
    check_hooks(&manifest.hooks, &record.exports).map_err(Rejection::invalid)?;
    if manifest.hooks.is_empty() {
        log::warn!("plugin `{}` lists no hooks and will never be called", record.id);
    }

    let lifecycle = lifecycle::GuestIndices::new(prepared.pre())
        .map_err(|err| Rejection::invalid(format!("cannot bind the lifecycle exports: {err:#}")))?;
    // Binds one export interface when the component has it.
    macro_rules! bind {
        ($module:ident, $name:literal) => {
            if record.exports.contains(&interface_name($name)) {
                Some(
                    $module::GuestIndices::new(prepared.pre())
                        .map_err(|err| Rejection::invalid(format!("cannot bind the {} exports: {err:#}", $name)))?,
                )
            } else {
                None
            }
        };
    }
    let playlist_hooks = bind!(playlist_hooks, "playlist-hooks");
    let source_hooks = bind!(source_hooks, "source-hooks");
    let sort_hooks = bind!(sort_hooks, "sort-hooks");
    let sort_key_hooks = bind!(sort_key_hooks, "sort-key-hooks");
    let chno_hooks = bind!(chno_hooks, "chno-hooks");
    let dedupe_hooks = bind!(dedupe_hooks, "dedupe-hooks");
    let group_merge_hooks = bind!(group_merge_hooks, "group-merge-hooks");
    let filter_hooks = bind!(filter_hooks, "filter-hooks");
    let epg_load_hooks = bind!(epg_load_hooks, "epg-load-hooks");
    let epg_transform_hooks = bind!(epg_transform_hooks, "epg-transform-hooks");
    let epg_enrich_hooks = bind!(epg_enrich_hooks, "epg-enrich-hooks");
    let epg_match_hooks = bind!(epg_match_hooks, "epg-match-hooks");
    let metadata_hooks = bind!(metadata_hooks, "metadata-hooks");
    let event_hooks = bind!(event_hooks, "event-hooks");
    let metrics_hooks = bind!(metrics_hooks, "metrics-hooks");
    let notification_hooks = bind!(notification_hooks, "notification-hooks");
    let input_provider = bind!(input_provider, "input-provider");
    let output_generator = bind!(output_generator, "output-generator");
    let stream_hooks = bind!(stream_hooks, "stream-hooks");
    let tasks = bind!(tasks, "tasks");
    let rpc = bind!(rpc, "rpc");
    let scope = PluginScope::new(&manifest.scope.groups, &manifest.scope.item_types)
        .map_err(|err| Rejection::invalid(format!("scope.groups: {err}")))?;

    let settings = ctx.config.resolved_settings(&record.id).unwrap_or_else(|| Value::Object(Map::new()));
    let settings_json = settings.to_string();
    let secrets = secret_values(&settings, secret_paths);
    let env = ctx
        .shared
        .env_for(&record.id, manifest.capabilities, Arc::from(settings_json.as_str()), Arc::from(secrets))
        .map_err(|err| Rejection::invalid(err.message()))?;

    let fingerprint =
        PluginFingerprint::new(&record.id, manifest.version.to_string(), wasm_sha256.to_string(), &settings_json);
    record.loaded = Some(LoadedPlugin {
        prepared,
        env,
        lifecycle,
        playlist_hooks,
        source_hooks,
        sort_hooks,
        sort_key_hooks,
        chno_hooks,
        dedupe_hooks,
        group_merge_hooks,
        filter_hooks,
        epg_load_hooks,
        epg_transform_hooks,
        epg_enrich_hooks,
        epg_match_hooks,
        metadata_hooks,
        event_hooks,
        metrics_hooks,
        notification_hooks,
        input_provider,
        output_generator,
        stream_hooks,
        tasks,
        rpc,
        hooks: manifest.pipeline_hooks(),
        scope: Arc::new(scope),
        fail_mode: manifest.resources.fail_mode,
        fingerprint,
    });
    record.state = state_for_entry(entry);
    Ok(())
}

/// Parse, check, approve and load one plugin directory into `record`. Returns
/// the pin to write when the config entry approved the plugin just now.
fn evaluate(
    ctx: &ScanContext<'_>,
    record: &mut PluginRecord,
    entry: Option<&PluginEntry>,
) -> Result<Option<ApprovalPin>, Rejection> {
    let text = fs::read_to_string(record.dir.join(MANIFEST_FILE))
        .map_err(|err| Rejection::invalid(format!("cannot read {MANIFEST_FILE}: {err}")))?;
    let manifest = Arc::new(Manifest::parse(&text, &record.id).map_err(|err| Rejection::invalid(err.message()))?);
    record.manifest = Some(Arc::clone(&manifest));
    if let Some(problem) = host_version_problem(&manifest) {
        return Err(Rejection::invalid(problem));
    }
    // Plan §4 steps 4-6: the schemas, the locale files and the extension sections.
    let secret_paths =
        check_package(&record.dir, &manifest).map_err(|problems| Rejection::invalid(problems.join("; ")))?;

    // A package without `plugin.wasm` is a UI-only plugin: nothing to compile,
    // so it may list no hook. Its pin carries an empty hash.
    let wasm = match fs::read(record.dir.join(WASM_FILE)) {
        Ok(wasm) => Some(wasm),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(Rejection::invalid(format!("cannot read {WASM_FILE}: {err}"))),
    };
    if wasm.is_none() && !manifest.hooks.is_empty() {
        return Err(Rejection::invalid(format!("hooks are listed in {MANIFEST_FILE} but {WASM_FILE} is missing")));
    }
    let wasm_sha256 = wasm.as_deref().map(sha256_hex);
    record.wasm_sha256.clone_from(&wasm_sha256);
    let pinned_hash = wasm_sha256.as_deref().unwrap_or_default();

    let capabilities = capability_names(manifest.capabilities);
    let pin = match decide_approval(
        ctx.approvals.get(&*record.id),
        pinned_hash,
        &capabilities,
        entry.is_some_and(|entry| entry.enabled),
    ) {
        Verdict::Approved => None,
        Verdict::PinNow => Some(ApprovalPin { wasm_sha256: pinned_hash.to_string(), capabilities, mcp: false }),
        Verdict::Pending(reason) => return Err(Rejection { state: PluginState::PendingApproval, reason }),
    };
    match (wasm, wasm_sha256) {
        (Some(wasm), Some(wasm_sha256)) => load(ctx, record, &manifest, &wasm, &wasm_sha256, &secret_paths, entry)?,
        _ => record.state = state_for_entry(entry),
    }
    Ok(pin)
}

/// Why this host is too old for the manifest's `min_tuliprox_version`, if it is.
pub(crate) fn host_version_problem(manifest: &Manifest) -> Option<String> {
    let host = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let minimum = manifest.min_tuliprox_version.as_ref()?;
    manifest.requires_newer_host(&host).then(|| format!("it needs tuliprox {minimum} or newer, this is {host}"))
}

fn build_record(ctx: &ScanContext<'_>, candidate: &Candidate) -> (PluginRecord, Option<ApprovalPin>) {
    let entry = ctx.config.entry(&candidate.id);
    let priority = entry.map_or(DEFAULT_PLUGIN_PRIORITY, |entry| entry.priority);
    let mut record = PluginRecord::new(&candidate.id, candidate.dir.clone(), priority);
    match evaluate(ctx, &mut record, entry) {
        Ok(pin) => {
            log::info!("plugin `{}` is {}", record.id, record.state.as_str());
            (record, pin)
        }
        Err(Rejection { state, reason }) => {
            log::warn!("plugin `{}` is {}: {reason}", record.id, state.as_str());
            record.state = state;
            record.failure = Some(reason);
            (record, None)
        }
    }
}

struct Scan {
    /// Sorted by (priority, id).
    plugins: Vec<Arc<PluginRecord>>,
    /// The wasm hashes of the plugins that loaded.
    kept: HashSet<String>,
}

/// Scan `config.directory`. `None` when the directory exists but cannot be
/// read, so a reload keeps what it had instead of dropping every plugin.
fn scan(config: &PluginsConfig, runtime: &Runtime, shared: &Shared) -> Option<Scan> {
    if !config.enabled {
        return Some(Scan { plugins: Vec::new(), kept: HashSet::new() });
    }
    let directory = Path::new(&config.directory);
    let candidates = match discover(directory) {
        Ok(candidates) => candidates,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            log::warn!("plugins are enabled but the directory {} does not exist", directory.display());
            Vec::new()
        }
        Err(err) => {
            log::error!("cannot read the plugins directory {}: {err}", directory.display());
            return None;
        }
    };

    let approvals_file = approvals_path(&shared.data_dir);
    let mut approvals = read_approvals(&approvals_file);
    let mut records = Vec::with_capacity(candidates.len());
    let mut new_pins = Vec::new();
    {
        let ctx = ScanContext { config, runtime, shared, approvals: &approvals };
        for candidate in &candidates {
            let (record, pin) = build_record(&ctx, candidate);
            if let Some(pin) = pin {
                new_pins.push((record.id.to_string(), pin));
            }
            records.push(record);
        }
    }
    if !new_pins.is_empty() {
        for (id, pin) in new_pins {
            log::info!("plugin `{id}` approved by config entry (trust on first use)");
            approvals.insert(id, pin);
        }
        if let Err(err) = write_approvals(&approvals_file, &approvals) {
            log::error!("cannot write the plugin approvals {}: {err}", approvals_file.display());
        }
    }

    for entry in config.plugins.iter().filter(|entry| !candidates.iter().any(|candidate| candidate.id == entry.id)) {
        log::warn!("plugins.plugins[] lists `{}`, but {} has no such plugin", entry.id, directory.display());
    }

    let kept = records
        .iter()
        .filter(|record| record.loaded.is_some())
        .filter_map(|record| record.wasm_sha256.clone())
        .collect();
    records.sort_by(|a, b| (a.priority, &a.id).cmp(&(b.priority, &b.id)));
    Some(Scan { plugins: records.into_iter().map(Arc::new).collect(), kept })
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// A running directory watcher and what it watches.
struct Watch {
    watcher: RecommendedWatcher,
    dir: PathBuf,
}

pub(crate) struct Registry {
    snapshot: ArcSwap<RegistrySnapshot>,
    runtime: Arc<Runtime>,
    shared: Arc<Shared>,
    /// The config of the last scan. Held for the whole of a scan, so scans
    /// (a reload and the watcher's) never overlap.
    config: Mutex<PluginsConfig>,
    /// Dropped with the registry, which ends the watcher thread.
    watch: Mutex<Option<Watch>>,
    /// Bumped after every snapshot swap, so the event bridge, the plugin
    /// notification channels and the providers can rebuild what they derived
    /// from the plugin set ([`Registry::changes`]).
    generation: tokio::sync::watch::Sender<u64>,
}

/// Whether a filesystem event can change what a scan sees. Reads (`Access`
/// opens) and metadata changes cannot, and scanning reads every file.
fn is_change(kind: EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any | EventKind::Other => true,
        EventKind::Modify(modify) => !matches!(modify, ModifyKind::Metadata(_)),
        EventKind::Access(access) => matches!(access, AccessKind::Close(AccessMode::Write)),
    }
}

/// Re-scan once the directory has been quiet for [`WATCH_DEBOUNCE`], until
/// the registry (and with it the event sender) is gone.
fn watch_loop(events: &mpsc::Receiver<()>, registry: &Weak<Registry>) {
    while events.recv().is_ok() {
        loop {
            match events.recv_timeout(WATCH_DEBOUNCE) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let Some(registry) = registry.upgrade() else {
            return;
        };
        log::info!("the plugins directory changed, re-scanning");
        registry.reload_remembered();
    }
}

impl Registry {
    /// Scan `config.directory` and load what is approved. Synchronous and
    /// CPU-bound (compiles components): call it from `spawn_blocking`.
    pub(crate) fn load(config: &PluginsConfig, runtime: &Arc<Runtime>, shared: &Arc<Shared>) -> Self {
        let registry = Self {
            snapshot: ArcSwap::from_pointee(RegistrySnapshot::default()),
            runtime: Arc::clone(runtime),
            shared: Arc::clone(shared),
            config: Mutex::new(config.clone()),
            watch: Mutex::new(None),
            generation: tokio::sync::watch::Sender::new(0),
        };
        registry.rescan(config);
        registry
    }

    pub(crate) fn snapshot(&self) -> Arc<RegistrySnapshot> { self.snapshot.load_full() }

    /// A receiver that sees a new value after every reload, whatever changed.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> { self.generation.subscribe() }

    /// The plugins directory of the last scan.
    pub(crate) fn directory(&self) -> PathBuf { PathBuf::from(&lock(&self.config).directory) }

    /// Change the approval pins on disk. Holds the scan lock, so a scan (a
    /// reload or the watcher's) never reads the file halfway, and never writes
    /// back a copy that predates this change. Synchronous: call it from a
    /// blocking thread, since a scan in progress holds the lock for as long as
    /// it compiles.
    pub(crate) fn update_approvals(&self, change: impl FnOnce(&mut Approvals)) -> io::Result<()> {
        let _scan_lock = lock(&self.config);
        let path = approvals_path(&self.shared.data_dir);
        let mut approvals = read_approvals(&path);
        change(&mut approvals);
        write_approvals(&path, &approvals)
    }

    /// Re-scan with `config` and swap the snapshot. Unchanged plugins keep
    /// their compiled component. Synchronous, like [`Registry::load`].
    pub(crate) fn reload(&self, config: &PluginsConfig) {
        let mut remembered = lock(&self.config);
        remembered.clone_from(config);
        self.rescan(&remembered);
        // Also when the directory did not change: a watch that failed earlier is retried here.
        self.rewatch(&remembered.directory);
    }

    /// Re-scan with the config of the last scan (the watcher's trigger).
    fn reload_remembered(&self) {
        let config = lock(&self.config);
        self.rescan(&config);
    }

    fn rescan(&self, config: &PluginsConfig) {
        let Some(scan) = scan(config, &self.runtime, &self.shared) else {
            return;
        };
        self.runtime.retain_cached(&scan.kept);
        let enabled = scan.plugins.iter().filter(|plugin| plugin.state() == PluginState::Enabled).count();
        log::info!("plugins: {} found, {enabled} enabled", scan.plugins.len());
        let batch_size = usize::try_from(config.batch_size).unwrap_or(usize::MAX).max(1);
        self.snapshot.store(Arc::new(RegistrySnapshot { plugins: scan.plugins, batch_size }));
        self.generation.send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Follow `directory` if the watcher is running and it is not the one
    /// watched. A failed `watch` keeps the watcher, watching nothing (an empty
    /// `dir`), so the next call tries again.
    fn rewatch(&self, directory: &str) {
        let mut guard = lock(&self.watch);
        let Some(active) = guard.as_mut() else {
            return;
        };
        let target = PathBuf::from(directory);
        if active.dir == target {
            return;
        }
        let _ = active.watcher.unwatch(&active.dir);
        match active.watcher.watch(&target, RecursiveMode::Recursive) {
            Ok(()) => active.dir = target,
            Err(err) => {
                log::warn!("failed to watch plugin directory {}: {err}", target.display());
                active.dir = PathBuf::new();
            }
        }
    }

    /// Re-scan whenever the plugins directory changes, until the registry is
    /// dropped.
    ///
    /// The watcher lives inside the registry and owns the sending end of a
    /// channel; a dedicated thread debounces it and holds the registry only
    /// weakly, so dropping the registry drops the watcher, which ends the
    /// thread.
    pub(crate) fn watch(self: &Arc<Self>) -> Result<(), TuliproxError> {
        let dir = PathBuf::from(&lock(&self.config).directory);
        if !dir.is_dir() {
            return Err(TuliproxError::Plugin(format!("the plugins directory {} does not exist", dir.display())));
        }
        let (sender, events) = mpsc::channel::<()>();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                if is_change(event.kind) {
                    let _ = sender.send(());
                }
            }
            Err(err) => log::warn!("plugins directory watcher: {err}"),
        })
        .map_err(|err| TuliproxError::Plugin(format!("cannot create the plugins directory watcher: {err}")))?;
        watcher
            .watch(&dir, RecursiveMode::Recursive)
            .map_err(|err| TuliproxError::Plugin(format!("cannot watch {}: {err}", dir.display())))?;

        let registry = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("plugin-watch".to_string())
            .spawn(move || watch_loop(&events, &registry))
            .map_err(|err| TuliproxError::Plugin(format!("cannot start the plugins watcher thread: {err}")))?;
        log::info!("watching {} for plugin changes", dir.display());
        // Replacing an earlier watcher drops its sender, which ends its thread.
        *lock(&self.watch) = Some(Watch { watcher, dir });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime::RuntimeConfig, state::PluginServices};
    use serde_json::json;
    use shared::model::PluginEntryDto;

    const MANIFEST: &str = "name: Demo\nversion: 0.1.0\napi_version: 1\nhooks: [playlist.after-filter]\n";

    fn manifest(id: &str, extra: &str) -> String { format!("id: {id}\n{MANIFEST}{extra}") }

    /// A plugin directory whose wasm is not a component: enough to reach every
    /// state before compilation.
    fn plugin_dir(root: &Path, id: &str, manifest_text: &str, wasm: Option<&[u8]>) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_FILE), manifest_text).unwrap();
        if let Some(wasm) = wasm {
            fs::write(dir.join(WASM_FILE), wasm).unwrap();
        }
    }

    fn config(directory: &Path) -> PluginsConfig {
        PluginsConfig { enabled: true, directory: directory.to_string_lossy().to_string(), ..PluginsConfig::default() }
    }

    fn entry(id: &str, enabled: bool, priority: i32) -> PluginEntry {
        let mut entry = PluginEntry::from(&PluginEntryDto::new(id));
        entry.enabled = enabled;
        entry.priority = priority;
        entry
    }

    struct Fixture {
        plugins: tempfile::TempDir,
        data: tempfile::TempDir,
        runtime: Arc<Runtime>,
        shared: Arc<Shared>,
    }

    impl Fixture {
        fn new() -> Self {
            let plugins = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let runtime = Arc::new(Runtime::new(RuntimeConfig::from_config(&PluginsConfig::default())).unwrap());
            let shared = Arc::new(Shared::new(PluginServices::new(reqwest::Client::new(), data.path().to_path_buf())));
            Self { plugins, data, runtime, shared }
        }

        fn registry(&self, config: &PluginsConfig) -> Registry { Registry::load(config, &self.runtime, &self.shared) }

        fn approvals_file(&self) -> PathBuf { approvals_path(self.data.path()) }
    }

    fn record_of(registry: &Registry, id: &str) -> Arc<PluginRecord> {
        Arc::clone(registry.snapshot().get(id).unwrap_or_else(|| panic!("no record for {id}")))
    }

    // --- discovery ---------------------------------------------------------

    #[test]
    fn discover_requires_a_manifest_skips_the_rest_and_sorts_by_id() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["b", "a"] {
            plugin_dir(dir.path(), id, "", Some(b""));
        }
        // no manifest, hidden, loose file: not candidates
        fs::create_dir(dir.path().join("c")).unwrap();
        plugin_dir(dir.path(), ".hidden", "", Some(b""));
        fs::write(dir.path().join("loose-file"), "").unwrap();
        // a manifest without a wasm file is still discovered (and then invalid)
        plugin_dir(dir.path(), "only-manifest", "", None);
        let ids: Vec<_> = discover(dir.path()).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["a", "b", "only-manifest"]);
    }

    #[test]
    fn a_missing_directory_gives_an_empty_snapshot_and_no_watcher() {
        let fixture = Fixture::new();
        let missing = fixture.plugins.path().join("absent");
        let registry = Arc::new(fixture.registry(&config(&missing)));
        assert!(registry.snapshot().plugins.is_empty());
        assert!(registry.watch().is_err());
    }

    #[test]
    fn disabled_plugins_config_yields_no_plugins() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), Some(b"x"));
        let mut disabled = config(fixture.plugins.path());
        disabled.enabled = false;
        assert!(fixture.registry(&disabled).snapshot().plugins.is_empty());
    }

    #[test]
    fn a_bad_manifest_is_invalid_with_the_reason() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "broken", "id: broken\nname: [", Some(b"x"));
        // the id must equal the directory name
        plugin_dir(fixture.plugins.path(), "renamed", &manifest("other", ""), Some(b"x"));
        let registry = fixture.registry(&config(fixture.plugins.path()));
        for id in ["broken", "renamed"] {
            let record = record_of(&registry, id);
            assert_eq!(record.state(), PluginState::Invalid, "{id}");
            assert!(record.manifest.is_none());
            assert!(record.failure().is_some_and(|reason| reason.contains("manifest.yml")), "{id}");
        }
    }

    #[test]
    fn a_missing_wasm_is_invalid() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), None);
        let record = record_of(&fixture.registry(&config(fixture.plugins.path())), "demo");
        assert_eq!(record.state(), PluginState::Invalid);
        assert!(record.manifest.is_some());
        assert!(record.wasm_sha256.is_none());
        assert!(record.failure().unwrap().contains(WASM_FILE));
    }

    #[test]
    fn a_secret_with_a_default_in_the_schema_is_invalid() {
        let fixture = Fixture::new();
        let extra = "ui:\n  config_section:\n    schema: config-schema.json\n";
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", extra), Some(b"x"));
        fs::write(
            fixture.plugins.path().join("demo/config-schema.json"),
            r#"{"type":"object","properties":{"token":{"type":"string","writeOnly":true,"default":"abc"}}}"#,
        )
        .unwrap();
        let record = record_of(&fixture.registry(&config(fixture.plugins.path())), "demo");
        assert_eq!(record.state(), PluginState::Invalid);
        assert!(record.failure().unwrap().contains("token"));
    }

    #[test]
    fn plugins_run_in_priority_then_id_order() {
        let fixture = Fixture::new();
        for id in ["delta", "alpha", "bravo", "charlie"] {
            plugin_dir(fixture.plugins.path(), id, &manifest(id, ""), Some(b"x"));
        }
        let mut config = config(fixture.plugins.path());
        config.plugins = vec![entry("delta", true, 1), entry("charlie", false, 100)];
        let registry = fixture.registry(&config);
        let ids: Vec<_> = registry.snapshot().plugins.iter().map(|plugin| plugin.id.to_string()).collect();
        // delta has priority 1; the rest default to 100 and sort by id
        assert_eq!(ids, ["delta", "alpha", "bravo", "charlie"]);
    }

    // --- approval ----------------------------------------------------------

    fn pin(wasm: &[u8], capabilities: &[&str]) -> ApprovalPin {
        ApprovalPin {
            wasm_sha256: sha256_hex(wasm),
            capabilities: capabilities.iter().map(ToString::to_string).collect(),
            mcp: false,
        }
    }

    #[test]
    fn decide_approval_follows_the_freeze() {
        let caps = vec!["network".to_string(), "storage".to_string()];
        // no pin: an enabled entry approves, anything else waits
        assert_eq!(decide_approval(None, "abc", &caps, true), Verdict::PinNow);
        assert!(
            matches!(decide_approval(None, "abc", &caps, false), Verdict::Pending(reason) if reason.contains("network"))
        );
        // a matching pin approves, whatever the entry says and however the pin is ordered
        let matching = ApprovalPin {
            wasm_sha256: "abc".into(),
            capabilities: vec!["storage".into(), "network".into()],
            mcp: false,
        };
        assert_eq!(decide_approval(Some(&matching), "abc", &caps, false), Verdict::Approved);
        assert_eq!(decide_approval(Some(&matching), "abc", &caps, true), Verdict::Approved);
        // a changed hash or capability set always waits, even for an enabled entry
        assert!(
            matches!(decide_approval(Some(&matching), "def", &caps, true), Verdict::Pending(r) if r.contains("changed"))
        );
        let fewer = vec!["network".to_string()];
        assert!(
            matches!(decide_approval(Some(&matching), "abc", &fewer, true), Verdict::Pending(r) if r.contains("capabilities"))
        );
        let both = decide_approval(Some(&matching), "def", &fewer, true);
        assert!(matches!(both, Verdict::Pending(r) if r.contains("plugin.wasm") && r.contains("capabilities")));
    }

    #[test]
    fn an_unpinned_plugin_without_an_enabled_entry_is_pending_and_not_compiled() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), Some(b"not wasm"));
        for entries in [vec![], vec![entry("demo", false, 100)]] {
            let mut config = config(fixture.plugins.path());
            config.plugins = entries;
            let record = record_of(&fixture.registry(&config), "demo");
            // pending, not invalid: the bad wasm was never compiled
            assert_eq!(record.state(), PluginState::PendingApproval);
            assert!(record.loaded.is_none());
            assert_eq!(record.wasm_sha256, Some(sha256_hex(b"not wasm")));
            assert!(record.failure().unwrap().contains("not approved"));
        }
        assert!(!fixture.approvals_file().exists());
    }

    #[test]
    fn a_matching_pin_approves_and_compiles() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), Some(b"not wasm"));
        let mut pins = Approvals::new();
        pins.insert("demo".into(), pin(b"not wasm", &[]));
        write_approvals(&fixture.approvals_file(), &pins).unwrap();
        let record = record_of(&fixture.registry(&config(fixture.plugins.path())), "demo");
        // approved: it got as far as compiling, which the fake wasm fails
        assert_eq!(record.state(), PluginState::Invalid);
        assert!(record.failure().unwrap().contains("cannot compile"));
    }

    #[test]
    fn a_changed_hash_or_capability_set_goes_back_to_pending() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "hash", &manifest("hash", ""), Some(b"new bytes"));
        plugin_dir(fixture.plugins.path(), "caps", &manifest("caps", "capabilities:\n  network: true\n"), Some(b"x"));
        let mut pins = Approvals::new();
        pins.insert("hash".into(), pin(b"old bytes", &[]));
        pins.insert("caps".into(), pin(b"x", &["storage"]));
        write_approvals(&fixture.approvals_file(), &pins).unwrap();
        let mut config = config(fixture.plugins.path());
        // the enabled entries do not re-approve a pin that no longer matches
        config.plugins = vec![entry("hash", true, 100), entry("caps", true, 100)];
        let registry = fixture.registry(&config);

        let hash = record_of(&registry, "hash");
        assert_eq!(hash.state(), PluginState::PendingApproval);
        assert!(hash.failure().unwrap().contains("plugin.wasm changed"));
        let caps = record_of(&registry, "caps");
        assert_eq!(caps.state(), PluginState::PendingApproval);
        assert!(caps.failure().unwrap().contains("approved: storage; now: network"));
        // and the pins are untouched
        assert_eq!(read_approvals(&fixture.approvals_file()), pins);
    }

    #[test]
    fn an_enabled_entry_pins_only_what_loaded() {
        let fixture = Fixture::new();
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), Some(b"not wasm"));
        let mut config = config(fixture.plugins.path());
        config.plugins = vec![entry("demo", true, 100)];
        let record = record_of(&fixture.registry(&config), "demo");
        // the entry approved it, but it does not compile: invalid, and not pinned
        assert_eq!(record.state(), PluginState::Invalid);
        assert!(!fixture.approvals_file().exists());
    }

    #[test]
    fn approvals_round_trip_atomically_and_a_corrupt_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = approvals_path(dir.path().join("nested").as_path());
        let mut pins = Approvals::new();
        pins.insert("demo".into(), pin(b"wasm", &["network"]));
        write_approvals(&path, &pins).unwrap();
        assert_eq!(read_approvals(&path), pins);
        // the on-disk shape is `id -> {wasm_sha256, capabilities}`
        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["demo"]["capabilities"], json!(["network"]));
        assert_eq!(raw["demo"]["wasm_sha256"], sha256_hex(b"wasm").as_str());
        // no temp file left behind
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::write(&path, "{not json").unwrap();
        assert!(read_approvals(&path).is_empty());
        assert!(read_approvals(&dir.path().join("missing.json")).is_empty());
    }

    #[test]
    fn reload_picks_up_new_plugins_and_a_new_config() {
        let fixture = Fixture::new();
        let registry = fixture.registry(&config(fixture.plugins.path()));
        assert!(registry.snapshot().plugins.is_empty());
        plugin_dir(fixture.plugins.path(), "demo", &manifest("demo", ""), Some(b"x"));
        let before = registry.snapshot();
        registry.reload(&config(fixture.plugins.path()));
        assert!(before.plugins.is_empty(), "an old snapshot never changes");
        assert_eq!(record_of(&registry, "demo").state(), PluginState::PendingApproval);
        registry.reload(&config(&fixture.plugins.path().join("elsewhere")));
        assert!(registry.snapshot().plugins.is_empty());
    }

    // --- state from the config entry --------------------------------------

    #[test]
    fn state_follows_the_config_entry() {
        assert_eq!(state_for_entry(Some(&entry("a", true, 100))), PluginState::Enabled);
        assert_eq!(state_for_entry(Some(&entry("a", false, 100))), PluginState::Disabled);
        assert_eq!(state_for_entry(None), PluginState::Loaded);
    }

    // --- exports and hooks -------------------------------------------------

    fn exports(names: &[&str]) -> Vec<String> { names.iter().map(|name| interface_name(name)).collect() }

    #[test]
    fn the_export_names_the_registry_builds_are_the_api_names() {
        assert_eq!(interface_name("lifecycle"), EXPORT_INTERFACES[0]);
        assert_eq!(interface_name(HookId::PlaylistAfterFilter.wit_interface()), EXPORT_INTERFACES[1]);
        for hook in HookId::ALL {
            assert!(EXPORT_INTERFACES.contains(&interface_name(hook.wit_interface()).as_str()), "{}", hook.as_str());
        }
    }

    #[test]
    fn split_exports_keeps_known_names_in_api_order_and_reports_the_rest() {
        let (known, unknown) = split_exports([
            "tuliprox:plugin/playlist-hooks@1.0.0",
            "wasi:cli/run@0.2.0",
            "tuliprox:plugin/lifecycle@1.0.0",
            "tuliprox:plugin/lifecycle@2.0.0",
        ]);
        assert_eq!(known, exports(&["lifecycle", "playlist-hooks"]));
        assert_eq!(unknown, ["wasi:cli/run@0.2.0", "tuliprox:plugin/lifecycle@2.0.0"]);
    }

    #[test]
    fn hooks_must_match_the_exports() {
        let stage = [HookId::PlaylistAfterFilter, HookId::PlaylistBeforeMap];
        // every stage id shares one interface
        assert_eq!(check_hooks(&stage, &exports(&["lifecycle", "playlist-hooks"])), Ok(()));
        // lifecycle only, nothing listed: fine
        assert_eq!(check_hooks(&[], &exports(&["lifecycle"])), Ok(()));
        // a listed hook without its interface
        let missing = check_hooks(&stage, &exports(&["lifecycle"])).unwrap_err();
        assert!(missing.contains("`playlist.after-filter`") && missing.contains("playlist-hooks@1.0.0"), "{missing}");
        // an exported hook interface nothing lists
        let unused = check_hooks(&[], &exports(&["lifecycle", "sort-hooks"])).unwrap_err();
        assert!(unused.contains("sort-hooks@1.0.0") && unused.contains("no hook"), "{unused}");
        // lifecycle is mandatory
        let no_lifecycle = check_hooks(&stage, &exports(&["playlist-hooks"])).unwrap_err();
        assert!(no_lifecycle.contains("lifecycle"), "{no_lifecycle}");
        // all problems at once
        let all = check_hooks(&[HookId::PlaylistLoaded], &exports(&["sort-hooks"])).unwrap_err();
        assert_eq!(all.matches("; ").count(), 2, "{all}");
    }

    // --- secrets -----------------------------------------------------------

    #[test]
    fn secret_values_follow_the_schema_paths() {
        let settings = json!({
            "token": "t0k",
            "credentials": {"password": "pw"},
            "servers": [{"key": "k1", "host": "a"}, {"key": "k2"}],
            "extra": {"x": "ex", "y": "ey"},
            "empty": "",
            "number": 7,
            "tags": ["s1", "s2"],
        });
        let paths: Vec<String> =
            ["token", "credentials.password", "servers[].key", "extra.*", "empty", "number", "tags[]", "absent.deep"]
                .into_iter()
                .map(ToString::to_string)
                .collect();
        assert_eq!(secret_values(&settings, &paths), ["t0k", "pw", "k1", "k2", "ex", "ey", "s1", "s2"]);
        assert_eq!(secret_values(&json!("root"), &["(root)".to_string()]), ["root"]);
        assert_eq!(secret_values(&settings, &[]), Vec::<String>::new());
    }

    // --- circuit breaker ---------------------------------------------------

    fn enabled_record() -> PluginRecord {
        let mut record = PluginRecord::new("demo", PathBuf::from("demo"), 100);
        record.state = PluginState::Enabled;
        record
    }

    fn trap() -> CallError { CallError::Trap("boom".to_string()) }

    #[test]
    fn five_consecutive_failures_trip_the_breaker_until_reset() {
        let record = enabled_record();
        for _ in 0..BREAKER_THRESHOLD - 1 {
            record.report(Err(&trap()));
        }
        assert_eq!(record.state(), PluginState::Enabled);
        record.report(Err(&trap()));
        assert_eq!(record.state(), PluginState::Failed);
        assert!(record.failure().unwrap().contains("boom"));
        // a call still in flight reports after the trip: counted, still failed
        record.report(Ok(()));
        assert_eq!(record.state(), PluginState::Failed);
        let stats = record.stats();
        assert_eq!((stats.calls, stats.failures), (6, 5));
        assert_eq!(stats.last_error.as_deref(), Some("trapped: boom"));
        assert_eq!(stats.breaker.state, CircuitState::Open);
        // no cool-down: it stays failed until reset
        assert!(record.stats().breaker.forced_open);
        record.reset_breaker();
        assert_eq!(record.state(), PluginState::Enabled);
    }

    #[test]
    fn a_success_resets_the_streak() {
        let record = enabled_record();
        for _ in 0..BREAKER_THRESHOLD - 1 {
            record.report(Err(&CallError::Deadline));
        }
        record.report(Ok(()));
        for _ in 0..BREAKER_THRESHOLD - 1 {
            record.report(Err(&CallError::OutOfMemory));
        }
        assert_eq!(record.state(), PluginState::Enabled);
        assert_eq!(record.stats().breaker.consecutive_failures, BREAKER_THRESHOLD - 1);
    }

    #[test]
    fn budget_errors_are_a_host_condition_and_never_count() {
        let record = enabled_record();
        for _ in 0..BREAKER_THRESHOLD * 2 {
            record.report(Err(&CallError::Budget { waited: Duration::from_millis(5) }));
        }
        assert_eq!(record.state(), PluginState::Enabled);
        let stats = record.stats();
        assert_eq!((stats.calls, stats.failures, stats.budget_skips), (0, 0, u64::from(BREAKER_THRESHOLD) * 2));
        assert_eq!(stats.last_error, None);
        assert_eq!(stats.breaker.consecutive_failures, 0);
    }

    #[test]
    fn only_an_enabled_record_can_read_as_failed() {
        let mut record = enabled_record();
        record.state = PluginState::Disabled;
        for _ in 0..BREAKER_THRESHOLD {
            record.report(Err(&trap()));
        }
        assert_eq!(record.state(), PluginState::Disabled);
        // runs() needs a loaded component too
        record.state = PluginState::Enabled;
        assert!(!record.runs(PluginHook::AfterFilter));
    }

    #[test]
    fn states_have_stable_names() {
        assert_eq!(PluginState::PendingApproval.as_str(), "pending_approval");
        assert_eq!(PluginState::Failed.as_str(), "failed");
    }

    #[test]
    fn only_content_changes_trigger_a_rescan() {
        use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};
        assert!(is_change(EventKind::Create(CreateKind::File)));
        assert!(is_change(EventKind::Remove(RemoveKind::Folder)));
        assert!(is_change(EventKind::Modify(ModifyKind::Data(DataChange::Content))));
        assert!(is_change(EventKind::Modify(ModifyKind::Name(RenameMode::To))));
        assert!(is_change(EventKind::Access(AccessKind::Close(AccessMode::Write))));
        // a scan reads every file; reads must not re-trigger it
        assert!(!is_change(EventKind::Access(AccessKind::Open(AccessMode::Read))));
        assert!(!is_change(EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))));
    }

    #[test]
    fn failed_rewatch_keeps_the_watcher_and_the_next_reload_retries() {
        let fixture = Fixture::new();
        let config = PluginsConfig { directory: fixture.plugins.path().display().to_string(), ..Default::default() };
        let registry = Arc::new(fixture.registry(&config));
        registry.watch().unwrap();

        let later = fixture.plugins.path().join("later");
        let missing = PluginsConfig { directory: later.display().to_string(), ..Default::default() };
        registry.reload(&missing);
        assert!(lock(&registry.watch).is_some(), "a failed watch must not drop the watcher");

        // The same directory again, now present: the reload watches it.
        std::fs::create_dir(&later).unwrap();
        registry.reload(&missing);
        assert_eq!(lock(&registry.watch).as_ref().map(|active| active.dir.clone()), Some(later));
    }
}
