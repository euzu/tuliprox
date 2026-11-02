//! The state every guest instance runs with, and the services it is built from.
//!
//! This file defines the shape of [`PluginEnv`] and [`HostState`], which the
//! modules that contribute to them (`host_fns`, `logs`, `kv`, `http`,
//! `image_cache`) depend on, so a change here affects all of them.

use crate::{
    host_fns::{PluginHostEnv, StoreHostState},
    http::{GuestHttpHooks, Outbound, PluginHttp},
    image_cache::{ImageCache, PluginImages},
    kv::KvStores,
    logs::LogRegistry,
    manifest::Capabilities,
    runtime::{POOL_CORE_INSTANCES_PER_SLOT, POOL_TABLES_PER_SLOT},
};
use arc_swap::ArcSwapOption;
use shared::{error::TuliproxError, model::Millis};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, Mutex, PoisonError},
};
use tokio::sync::RwLock;
use tuliprox_core::utils::{Clock, LRUResourceCache, ManualClock, SystemClock};
use wasmtime::{component::ResourceTable, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// The clock behind `now-unix`. An enum rather than a generic so the whole
/// host stays one concrete type; tests freeze time with [`HostClock::Manual`].
#[derive(Clone)]
pub enum HostClock {
    System(SystemClock),
    /// Tests: plugins and host-side timing both follow this clock.
    Manual(ManualClock),
    /// Determinism checks on a running server: plugins see this frozen time,
    /// while host-side timing (throttles, TTL caches, log stamps) keeps the
    /// wall clock, so nothing host-side stalls.
    Frozen(ManualClock),
}

impl Default for HostClock {
    fn default() -> Self { Self::System(SystemClock) }
}

impl Clock for HostClock {
    fn now_ms(&self) -> Millis {
        match self {
            Self::System(clock) => clock.now_ms(),
            Self::Manual(clock) | Self::Frozen(clock) => clock.now_ms(),
        }
    }
}

impl HostClock {
    /// The time host-side timing runs on: the wall clock, except under
    /// [`HostClock::Manual`], where tests drive it.
    pub fn wall_ms(&self) -> Millis {
        match self {
            Self::Manual(clock) => clock.now_ms(),
            Self::System(_) | Self::Frozen(_) => SystemClock.now_ms(),
        }
    }
}

/// Seals an upstream URL into a path on the existing resource route
/// (`/api/v1/playlist/resource/{sealed}`). Supplied by `app`, which owns the
/// secret.
pub type SealResourceUrl = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// What `app` hands the plugin system at startup.
pub struct PluginServices {
    /// The dedicated plugin client (`create_plugin_http_client_no_redirect`):
    /// no redirects, no resource destination resolver.
    pub client: reqwest::Client,
    /// Per-plugin persistent data (`<storage_dir>/plugins`): KV files and the
    /// approval pins.
    pub data_dir: PathBuf,
    /// `AppState.cache`; `None` inside means the resource cache is disabled.
    pub resource_cache: Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
    pub seal_resource_url: SealResourceUrl,
    pub clock: HostClock,
}

impl PluginServices {
    /// The resource cache disabled, a sealer that refuses (cache-image then
    /// returns `Err`), the system clock.
    #[must_use]
    pub fn new(client: reqwest::Client, data_dir: PathBuf) -> Self {
        Self {
            client,
            data_dir,
            resource_cache: Arc::new(ArcSwapOption::empty()),
            seal_resource_url: Arc::new(|_url: &str| String::new()),
            clock: HostClock::default(),
        }
    }

    #[must_use]
    pub fn with_resource_cache(
        mut self,
        cache: Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
        seal_resource_url: SealResourceUrl,
    ) -> Self {
        self.resource_cache = cache;
        self.seal_resource_url = seal_resource_url;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: HostClock) -> Self {
        self.clock = clock;
        self
    }
}

/// The services shared by every plugin, built once from [`PluginServices`].
/// They outlive registry reloads, so a reload never reopens a KV file, drops a
/// log buffer or resets an executor key.
pub(crate) struct Shared {
    pub(crate) data_dir: PathBuf,
    pub(crate) clock: HostClock,
    pub(crate) outbound: Outbound,
    pub(crate) images: ImageCache,
    pub(crate) logs: LogRegistry,
    pub(crate) kv: KvStores,
    /// One `kv_generation` per plugin id, kept across reloads like the KV
    /// handles ([`PluginEnv::kv_generation`]).
    kv_generations: Mutex<HashMap<Arc<str>, Arc<AtomicU64>>>,
}

impl Shared {
    pub(crate) fn new(services: PluginServices) -> Self {
        let PluginServices { client, data_dir, resource_cache, seal_resource_url, clock } = services;
        Self {
            kv: KvStores::new(data_dir.clone()),
            data_dir,
            clock,
            outbound: Outbound::new(client),
            images: ImageCache::new(resource_cache, seal_resource_url),
            logs: LogRegistry::default(),
            kv_generations: Mutex::new(HashMap::new()),
        }
    }

    fn kv_generation(&self, id: &Arc<str>) -> Arc<AtomicU64> {
        let mut generations = self.kv_generations.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(generations.entry(Arc::clone(id)).or_default())
    }

    /// The environment of one loaded plugin. Called by the registry on every
    /// (re)load; `settings` are already `${env:…}`-resolved and `secrets` are
    /// the resolved values of the schema's secret properties.
    pub(crate) fn env_for(
        &self,
        id: &Arc<str>,
        capabilities: Capabilities,
        settings: Arc<str>,
        secrets: Arc<[String]>,
    ) -> Result<Arc<PluginEnv>, TuliproxError> {
        let log = self.logs.plugin(id);
        let kv = capabilities.storage.then(|| self.kv.handle(id));
        let http = PluginHttp::new(&self.outbound, id, capabilities.network)?;
        let images = PluginImages::new(&self.images, id, capabilities.image_cache && capabilities.network);
        Ok(Arc::new(PluginEnv {
            id: Arc::clone(id),
            settings,
            secrets,
            capabilities,
            clock: self.clock.clone(),
            host: PluginHostEnv::new(log, kv),
            http,
            images,
            kv_generation: self.kv_generation(id),
        }))
    }
}

/// Everything one plugin's instances share. Rebuilt on reload.
pub(crate) struct PluginEnv {
    pub(crate) id: Arc<str>,
    /// The settings JSON with secrets resolved. Only `get-config()` inside this
    /// plugin's own instances sees it.
    pub(crate) settings: Arc<str>,
    /// Resolved secret values, redacted from the plugin's log lines.
    pub(crate) secrets: Arc<[String]>,
    pub(crate) capabilities: Capabilities,
    pub(crate) clock: HostClock,
    pub(crate) host: PluginHostEnv,
    pub(crate) http: PluginHttp,
    pub(crate) images: PluginImages,
    /// Bumped by every task run that may have written KV. Later hook results can
    /// differ, so the
    /// plugin fingerprint includes it. In memory; a restart starts at 0.
    pub(crate) kv_generation: Arc<AtomicU64>,
}

/// Per-`Store` limits, for the on-demand allocator (pool fallback, armv7), which
/// the pooling allocator's per-slot limits in `runtime.rs` do not cover. Memories
/// match `max_memories_per_component`, instances and tables its per-slot sizes.
const STORE_LIMIT_TABLE_ELEMENTS: usize = 10_000;
const STORE_LIMIT_MEMORIES: usize = 1;

/// Per-`Store` state: a fresh one per guest instance.
pub(crate) struct HostState {
    pub(crate) env: Arc<PluginEnv>,
    pub(crate) wasi: WasiCtx,
    pub(crate) http_ctx: WasiHttpCtx,
    pub(crate) http_hooks: GuestHttpHooks,
    pub(crate) table: ResourceTable,
    pub(crate) limits: StoreLimits,
    pub(crate) host: StoreHostState,
}

impl HostState {
    /// `memory_bytes` is the plugin's clamped linear-memory limit.
    pub(crate) fn new(env: Arc<PluginEnv>, memory_bytes: usize) -> Self {
        Self {
            http_hooks: GuestHttpHooks::new(&env.http),
            host: StoreHostState::new(),
            env,
            // Empty on purpose: no preopened directories, environment or
            // arguments, stdio discarded, sockets denied. The linker still
            // provides all of `wasi:*` because a `wasm32-wasip2` guest imports
            // it even when unused; `random` and `clocks` stay functional.
            wasi: WasiCtx::builder().build(),
            http_ctx: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_bytes)
                .memories(STORE_LIMIT_MEMORIES)
                .tables(usize::try_from(POOL_TABLES_PER_SLOT).unwrap_or(usize::MAX))
                .instances(usize::try_from(POOL_CORE_INSTANCES_PER_SLOT).unwrap_or(usize::MAX))
                .table_elements(STORE_LIMIT_TABLE_ELEMENTS)
                .trap_on_grow_failure(true)
                .build(),
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx: &mut self.wasi, table: &mut self.table } }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView { ctx: &mut self.http_ctx, table: &mut self.table, hooks: &mut self.http_hooks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::ResourceLimiter;

    #[test]
    fn host_state_store_limits_are_configured() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Shared::new(PluginServices::new(reqwest::Client::new(), dir.path().to_path_buf())));
        let env = shared.env_for(&Arc::from("p"), Capabilities::default(), Arc::from("{}"), Arc::from([])).unwrap();
        let mut state = HostState::new(env, 1024 * 1024);

        assert_eq!(state.limits.instances(), 8, "POOL_CORE_INSTANCES_PER_SLOT");
        assert_eq!(state.limits.tables(), 4, "POOL_TABLES_PER_SLOT");
        assert_eq!(state.limits.memories(), 1);
        assert!(state.limits.table_growing(0, 10_000, None).unwrap());
        assert!(state.limits.table_growing(0, 10_001, None).is_err());
    }

    /// `(module (table 1 funcref) (func (export "grow") (param i32) (result i32)
    /// (table.grow 0 (ref.null func) (local.get 0))))`. A Rust guest cannot emit
    /// `table.grow` (no stable intrinsic), so the flood is a core module.
    const TABLE_GROW: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
        0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f, // type: (i32) -> i32
        0x03, 0x02, 0x01, 0x00, // function 0 has type 0
        0x04, 0x04, 0x01, 0x70, 0x00, 0x01, // table 0: funcref, min 1
        0x07, 0x08, 0x01, 0x04, b'g', b'r', b'o', b'w', 0x00, 0x00, // export "grow"
        0x0a, 0x0b, 0x01, 0x09, 0x00, 0xd0, 0x70, 0x20, 0x00, 0xfc, 0x0f, 0x00, 0x0b, // table.grow
    ];

    #[test]
    fn a_guest_cannot_flood_a_table_past_the_store_limit() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Shared::new(PluginServices::new(reqwest::Client::new(), dir.path().to_path_buf())));
        let env = shared.env_for(&Arc::from("p"), Capabilities::default(), Arc::from("{}"), Arc::from([])).unwrap();
        let engine = wasmtime::Engine::default();
        let mut store = wasmtime::Store::new(&engine, HostState::new(env, 1024 * 1024));
        store.limiter(|state| &mut state.limits);
        let module = wasmtime::Module::new(&engine, TABLE_GROW).expect("module");
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
        let grow = instance.get_typed_func::<i32, i32>(&mut store, "grow").expect("grow");

        // 1 + 9_000 elements is within the 10_000 limit; the next 1_000 are not, and neither is a flood.
        assert_eq!(grow.call(&mut store, 9_000).expect("within the limit"), 1);
        assert!(grow.call(&mut store, 1_000).is_err(), "10_001 elements are over the limit");
        assert!(grow.call(&mut store, i32::MAX).is_err(), "a flood is refused, not allocated");
    }

    #[test]
    fn a_frozen_clock_keeps_plugin_time_fixed_while_wall_time_advances() {
        let manual = ManualClock::new(1_793_448_000_000);
        let clock = HostClock::Frozen(manual.clone());
        let (plugin_before, wall_before) = (clock.now_ms().get(), clock.wall_ms().get());

        std::thread::sleep(std::time::Duration::from_millis(20));

        assert_eq!(clock.now_ms().get(), plugin_before, "plugins keep seeing the frozen instant");
        assert_eq!(plugin_before, 1_793_448_000_000);
        assert!(clock.wall_ms().get() >= wall_before + 20, "host-side timing follows the wall clock");
        assert_ne!(clock.wall_ms().get(), plugin_before, "wall time is not the frozen instant");

        // Only an explicit move changes the frozen plugin time.
        manual.advance(5);
        assert_eq!(clock.now_ms().get(), plugin_before + 5);
    }

    #[test]
    fn manual_drives_both_clocks_and_system_follows_the_wall_clock() {
        let manual = HostClock::Manual(ManualClock::new(42));
        assert_eq!((manual.now_ms().get(), manual.wall_ms().get()), (42, 42));

        let system = HostClock::default();
        let (now, wall) = (system.now_ms().get(), system.wall_ms().get());
        assert!(wall.abs_diff(now) < 5_000, "system plugin time and wall time are the same clock");
    }
}
