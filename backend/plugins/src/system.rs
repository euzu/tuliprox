use crate::{
    dispatch::HookDispatcher,
    epg_hooks,
    events::EventQueue,
    filter_batch,
    header_inject::HeaderCache,
    input_provider,
    mcp_tools::McpToolCache,
    metrics::MetricsCache,
    output_provider,
    registry::Registry,
    runtime::{Runtime, RuntimeConfig},
    state::{PluginServices, Shared},
    strategy,
    tasks::TaskRunner,
};
use shared::{
    error::TuliproxError,
    model::{
        EpgProgramme, HookCtx, PlaylistItem, PluginAbort, PluginChannelProgrammes, PluginChnoMap, PluginDuplicateGroup,
        PluginEpgChannel, PluginGroupMerge, PluginHook, PluginHost, PluginInputRequest, PluginKeepers,
        PluginMatchRequest, PluginOutputRequest, PluginRunReport, PluginScopes, PluginSortKeys,
    },
};
use std::sync::Arc;
use tuliprox_core::model::PluginsConfig;

struct Inner {
    /// Serializes upload, approve, enable, disable, uninstall and settings saves.
    install: tokio::sync::Mutex<()>,
    registry: Arc<Registry>,
    shared: Arc<Shared>,
    // Per-module state, built once
    // at load; a reload swaps the registry snapshot, never these.
    events: EventQueue,
    metrics: MetricsCache,
    headers: HeaderCache,
    tasks: TaskRunner,
    managed: crate::managed_channels::ManagedSnapshots,
    /// The MCP tools derived from the registry snapshot.
    mcp_tools: McpToolCache,
    // Declared last: the engine and its epoch ticker go after the registry,
    // which holds compiled components of this engine.
    runtime: Arc<Runtime>,
}

/// The wasm plugin host. Cheap to clone; all clones share one engine, one epoch
/// ticker thread and one registry.
///
/// Every hook invocation runs each plugin in a fresh instance, created lazily
/// (only for plugins that run that hook and whose scope matches an item) and
/// dropped at the end, so no state leaks across runs. There is no
/// target-run scope yet, so an instance lives for one hook call over one input,
/// not for the whole target run.
#[derive(Clone)]
pub struct PluginSystem {
    inner: Arc<Inner>,
}

impl PluginSystem {
    /// Start the plugin system and load the plugins under `config.directory`.
    ///
    /// Returns `None` when plugins are disabled. Plugins that fail to load are
    /// recorded with their reason (`Invalid`, `PendingApproval`) and never stop
    /// the others or the server. Later directory changes are picked up by the
    /// registry's watcher.
    ///
    /// This compiles components, which is CPU-bound and synchronous: call it
    /// from `tokio::task::spawn_blocking`.
    pub fn load(config: &PluginsConfig, services: PluginServices) -> Result<Option<Self>, TuliproxError> {
        if !config.enabled {
            return Ok(None);
        }
        let runtime = Arc::new(Runtime::new(RuntimeConfig::from_config(config))?);
        let shared = Arc::new(Shared::new(services));
        let registry = Arc::new(Registry::load(config, &runtime, &shared));
        if let Err(err) = registry.watch() {
            log::warn!("plugins will not hot reload: {err}");
        }
        Ok(Some(Self {
            inner: Arc::new(Inner {
                install: tokio::sync::Mutex::new(()),
                registry,
                managed: crate::managed_channels::ManagedSnapshots::new(shared.data_dir.join(".managed-channels")),
                shared,
                events: EventQueue::new(config),
                metrics: MetricsCache::new(config),
                headers: HeaderCache::new(config),
                tasks: TaskRunner::new(config),
                mcp_tools: McpToolCache::default(),
                runtime,
            }),
        }))
    }

    /// Re-scan with a changed `plugins:` config (config hot reload).
    /// Synchronous like [`PluginSystem::load`].
    pub fn reload(&self, config: &PluginsConfig) {
        self.inner.headers.set_ttl(config);
        self.inner.registry.reload(config);
    }

    /// Ids of every plugin directory found, in run order.
    pub fn plugin_ids(&self) -> Vec<String> {
        self.inner.registry.snapshot().plugins.iter().map(|plugin| plugin.id.to_string()).collect()
    }

    pub(crate) fn install_mutex(&self) -> &tokio::sync::Mutex<()> { &self.inner.install }

    pub(crate) fn registry(&self) -> &Arc<Registry> { &self.inner.registry }

    pub(crate) fn runtime(&self) -> &Arc<Runtime> { &self.inner.runtime }

    pub(crate) fn shared(&self) -> &Arc<Shared> { &self.inner.shared }

    /// A receiver that sees a new value after every registry reload (config
    /// reload, settings save, directory change). The event bridge rebuilds
    /// its mask on it, and the app invalidates messaging's channel set.
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> { self.inner.registry.changes() }

    pub(crate) fn events(&self) -> &EventQueue { &self.inner.events }

    pub(crate) fn metrics(&self) -> &MetricsCache { &self.inner.metrics }

    pub(crate) fn headers(&self) -> &HeaderCache { &self.inner.headers }

    pub(crate) fn tasks(&self) -> &TaskRunner { &self.inner.tasks }

    pub(crate) fn mcp_tool_cache(&self) -> &McpToolCache { &self.inner.mcp_tools }
}

impl PluginSystem {
    pub(crate) fn dispatcher(&self) -> HookDispatcher<'_> {
        HookDispatcher::new(&self.inner.runtime, self.inner.registry.snapshot())
    }
}

/// Every hook goes through the [`HookDispatcher`]: scope pre-filter, batching,
/// priority chaining, the sparse merge and stats. Fail-open unless a plugin
/// asks for `fail_mode: abort-target`.
impl PluginHost for PluginSystem {
    fn wants(&self, hook: PluginHook, _target: &str) -> bool { self.dispatcher().wants(hook) }

    fn scope(&self, hook: PluginHook, _ctx: &HookCtx<'_>) -> Option<PluginScopes> { self.dispatcher().scope(hook) }

    async fn on_stage(
        &self,
        hook: PluginHook,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        if items.is_empty() {
            return Ok(());
        }
        self.dispatcher().run_playlist_hook(hook, ctx, items, report).await
    }

    async fn on_playlist_loaded(
        &self,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        if items.is_empty() {
            return Ok(());
        }
        self.dispatcher().run_playlist_loaded(ctx, items, report).await
    }

    async fn managed_channels(
        &self,
        ctx: &HookCtx<'_>,
        report: &mut PluginRunReport,
    ) -> Result<Vec<PlaylistItem>, PluginAbort> {
        self.inner.managed.reconcile(&self.dispatcher(), ctx, report).await
    }

    fn provides(&self, hook: PluginHook, plugin: &str) -> bool { self.dispatcher().named(hook, plugin).is_some() }

    fn fingerprint(&self, hook: PluginHook) -> Option<String> { self.dispatcher().fingerprint(hook) }

    async fn after_sort(
        &self,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<Vec<usize>>, PluginAbort> {
        if items.is_empty() {
            return Ok(None);
        }
        strategy::after_sort(&self.dispatcher(), ctx, items, report).await
    }

    async fn sort_keys(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginSortKeys>, PluginAbort> {
        strategy::sort_keys(&self.dispatcher(), plugin, ctx, items, report).await
    }

    async fn assign_chno(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginChnoMap>, PluginAbort> {
        strategy::assign_chno(&self.dispatcher(), plugin, ctx, items, report).await
    }

    async fn resolve_duplicates(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        groups: &[PluginDuplicateGroup<'_>],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginKeepers>, PluginAbort> {
        strategy::resolve_duplicates(&self.dispatcher(), plugin, ctx, groups, report).await
    }

    async fn merge_groups(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        groups: &[String],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginGroupMerge>, PluginAbort> {
        strategy::merge_groups(&self.dispatcher(), plugin, ctx, groups, report).await
    }

    async fn eval_filter_batch(
        &self,
        plugin: &str,
        func: &str,
        args: &[String],
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Vec<bool> {
        filter_batch::eval_filter_batch(&self.dispatcher(), plugin, func, args, ctx, items, report).await
    }

    async fn on_epg_loaded(
        &self,
        ctx: &HookCtx<'_>,
        channels: &mut Vec<PluginEpgChannel>,
        report: &mut PluginRunReport,
    ) {
        epg_hooks::on_epg_loaded(&self.dispatcher(), ctx, channels, report).await;
    }

    async fn transform_programmes(
        &self,
        ctx: &HookCtx<'_>,
        programmes: &mut Vec<EpgProgramme>,
        report: &mut PluginRunReport,
    ) {
        epg_hooks::transform_programmes(&self.dispatcher(), ctx, programmes, report).await;
    }

    async fn enrich_programmes(
        &self,
        ctx: &HookCtx<'_>,
        channels: &mut [PluginChannelProgrammes],
        report: &mut PluginRunReport,
    ) {
        epg_hooks::enrich_programmes(&self.dispatcher(), ctx, channels, report).await;
    }

    async fn match_channels(
        &self,
        ctx: &HookCtx<'_>,
        requests: &[PluginMatchRequest<'_>],
        report: &mut PluginRunReport,
    ) -> Vec<Option<String>> {
        epg_hooks::match_channels(&self.dispatcher(), ctx, requests, report).await
    }

    async fn enrich_metadata(
        &self,
        ctx: &HookCtx<'_>,
        item: &PlaylistItem,
        metadata_json: &str,
        report: &mut PluginRunReport,
    ) -> Option<String> {
        epg_hooks::enrich_metadata(&self.dispatcher(), ctx, item, metadata_json, report).await
    }

    async fn fetch_playlist(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginInputRequest<'_>,
    ) -> Result<Vec<PlaylistItem>, String> {
        input_provider::fetch_playlist(&self.dispatcher(), plugin, ctx, request).await
    }

    async fn fetch_epg(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginInputRequest<'_>,
    ) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String> {
        input_provider::fetch_epg(&self.dispatcher(), plugin, ctx, request).await
    }

    fn output_format(&self, plugin: &str) -> Option<String> {
        output_provider::output_format(&self.dispatcher(), plugin)
    }

    async fn render_output(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginOutputRequest<'_>,
        items: &[&PlaylistItem],
    ) -> Result<(), String> {
        output_provider::render_output(&self.dispatcher(), plugin, ctx, request, items).await
    }
}
