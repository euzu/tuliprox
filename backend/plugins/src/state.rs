//! The state every guest instance runs with, and the services it is built from.
//!
//! Frozen for W3 (plan §19.7 briefs reference it): each lane owns the types it
//! contributes here (`host_fns`, `logs`, `kv`, `http`, `image_cache`) but not
//! this file, so the shape of [`PluginEnv`] and [`HostState`] changes only
//! through the integration owner.

use crate::{
    host_fns::{PluginHostEnv, StoreHostState},
    http::{GuestHttpHooks, Outbound, PluginHttp},
    image_cache::{ImageCache, PluginImages},
    kv::KvStores,
    logs::LogRegistry,
    manifest::Capabilities,
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
    /// Bumped by every task run that may have written KV (plan §7 *Scheduled
    /// plugin tasks*, *Determinism*): later hook results can differ, so the
    /// plugin fingerprint includes it. In memory; a restart starts at 0.
    pub(crate) kv_generation: Arc<AtomicU64>,
}

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
            limits: StoreLimitsBuilder::new().memory_size(memory_bytes).trap_on_grow_failure(true).build(),
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
