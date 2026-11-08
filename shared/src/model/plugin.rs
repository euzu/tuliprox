//! The interface between the playlist pipeline and a plugin host.
//!
//! The pipeline is generic over [`PluginHost`] the same way it is generic over
//! `EventSink` and `MetadataUpdateSink`: a bound, not a trait object. Runs
//! without plugins monomorphise against [`NoPlugins`], whose hooks are empty
//! and whose `wants` is constant `false`, so the hook sites compile to nothing.
//! The wasm host that implements the trait lives in `tuliprox-plugins`, which
//! this crate never names.

use super::{EpgProgramme, PlaylistItem, PlaylistItemType};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

/// A pipeline point a plugin host can be asked to run.
///
/// The wire id ([`PluginHook::as_str`]) is the WIT case name
/// (`playlist-hooks.stage`, or the interface for the non-stage hooks). The
/// manifest `hooks:` entry is the dotted form (`playlist.after-filter`), mapped
/// in `tuliprox-plugins`' manifest module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginHook {
    /// After an input downloaded, before target processing (`source-hooks`).
    /// The caller hands over only the scoped subset ([`PluginHost::scope`]).
    PlaylistLoaded,
    /// Plugin-owned channel snapshot merged into an existing input.
    ManagedChannels,
    /// Around each transform stage of a target, wherever
    /// `execution_plan.transform_stages` puts it (`playlist-hooks`).
    BeforeFilter,
    AfterFilter,
    BeforeRename,
    AfterRename,
    BeforeMap,
    AfterMap,
    /// Once per finalized view, between sort and channel numbering (`sort-hooks`).
    AfterSort,
    /// Per-target strategies, opt-in by target config naming one plugin
    /// (prepare -> apply).
    SortKey,
    AssignChno,
    ResolveDuplicates,
    MergeGroups,
    /// `plugin.<id>.<fn>(…)` filter terms (`filter-hooks`).
    FilterEval,
    /// EPG hooks (`epg-*-hooks`).
    EpgLoaded,
    TransformProgrammes,
    EnrichProgrammes,
    MatchChannel,
    /// Inside `update_vod_metadata` / `update_series_metadata` (`metadata-hooks`).
    EnrichMetadata,
    /// Hooks outside the per-target pipeline. The pipeline never asks `wants`
    /// for them;
    /// they mark which loaded plugins export the interface.
    /// Lifecycle events from the bus (`event-hooks`).
    OnEvent,
    /// Dashboard metrics (`metrics-hooks`).
    CollectMetrics,
    /// A `Channel::Plugin` delivery (`notification-hooks`).
    SendNotification,
    /// A `type: plugin` input (`input-provider`).
    FetchInput,
    /// A `type: plugin` target output (`output-generator`).
    RenderOutput,
    /// Extra upstream headers at stream-session setup (`stream-hooks`).
    InjectHeaders,
    /// A scheduled or manual plugin task (`tasks`).
    RunTask,
}

impl PluginHook {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlaylistLoaded => "playlist-loaded",
            Self::ManagedChannels => "managed-channels",
            Self::BeforeFilter => "before-filter",
            Self::AfterFilter => "after-filter",
            Self::BeforeRename => "before-rename",
            Self::AfterRename => "after-rename",
            Self::BeforeMap => "before-map",
            Self::AfterMap => "after-map",
            Self::AfterSort => "after-sort",
            Self::SortKey => "sort-key",
            Self::AssignChno => "assign-channel-numbers",
            Self::ResolveDuplicates => "resolve-duplicates",
            Self::MergeGroups => "merge-groups",
            Self::FilterEval => "eval-filter-batch",
            Self::EpgLoaded => "on-epg-loaded",
            Self::TransformProgrammes => "transform-programmes",
            Self::EnrichProgrammes => "enrich-programmes",
            Self::MatchChannel => "match-channels",
            Self::EnrichMetadata => "enrich-metadata",
            Self::OnEvent => "on-event",
            Self::CollectMetrics => "collect-metrics",
            Self::SendNotification => "send-notification",
            Self::FetchInput => "fetch-input",
            Self::RenderOutput => "render-output",
            Self::InjectHeaders => "inject-upstream-headers",
            Self::RunTask => "run-task",
        }
    }

    /// The six stage hooks, the only ones [`PluginHost::on_stage`] runs.
    #[must_use]
    pub const fn is_stage(self) -> bool {
        matches!(
            self,
            Self::BeforeFilter
                | Self::AfterFilter
                | Self::BeforeRename
                | Self::AfterRename
                | Self::BeforeMap
                | Self::AfterMap
        )
    }
}

/// An XMLTV channel as `on-epg-loaded` and `match-channel` see it. The id is
/// compared case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginEpgChannel {
    pub id: String,
    /// XMLTV display-names.
    pub names: Vec<String>,
    pub icon: Option<String>,
}

/// One EPG channel's programmes, handed to `enrich-programmes` and replaced
/// by what the plugins return.
#[derive(Debug, Clone)]
pub struct PluginChannelProgrammes {
    pub channel_id: Arc<str>,
    pub programmes: Vec<EpgProgramme>,
}

/// One channel the built-in EPG matcher left unresolved, with its top-K
/// shortlist from the built-in index.
#[derive(Debug, Clone)]
pub struct PluginMatchRequest<'a> {
    pub item: &'a PlaylistItem,
    pub candidates: Vec<PluginEpgChannel>,
}

/// What a `type: plugin` input hands its provider (`input-provider.input-request`).
#[derive(Debug, Clone, Copy)]
pub struct PluginInputRequest<'a> {
    pub input_name: &'a str,
    pub url: &'a str,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub options: &'a [(String, String)],
}

/// One `type: plugin` target output (`output-generator.output-request`). The
/// host writes the rendered bytes to `path` (temp file, then rename).
#[derive(Debug, Clone, Copy)]
pub struct PluginOutputRequest<'a> {
    pub target: &'a str,
    /// The plugin's `provides.output_format`.
    pub format: &'a str,
    pub options: &'a [(String, String)],
    pub path: &'a Path,
}

/// Flattens an input's or output's `plugin_config` into the `options` pairs of
/// `input-request` / `output-request`: a JSON string keeps its text, anything
/// else is its JSON text, and a nested object is flattened with dotted keys.
/// The order is the map's.
///
/// A key never contains a dot, so a flattened `a.b` always means the nested
/// `b` of `a` and never a literal `a.b`. An empty object contributes no pair,
/// and neither does a `null` (a YAML key left blank), so the plugin sees the
/// option as unset and applies its default rather than receiving `"null"`.
#[must_use]
pub fn plugin_options(config: &serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> {
    fn flatten(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
        if let serde_json::Value::Object(map) = value {
            for (key, child) in map {
                let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                flatten(&path, child, out);
            }
            return;
        }
        let text = match value {
            serde_json::Value::Null => return,
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        out.push((prefix.to_owned(), text));
    }

    let mut options = Vec::with_capacity(config.len());
    for (key, value) in config {
        flatten(key, value, &mut options);
    }
    options
}

/// `sort-key` result: one key per item, index-aligned.
pub type PluginSortKeys = Vec<String>;
/// `assign-channel-numbers` result: `(index into items, chno)` pairs.
pub type PluginChnoMap = Vec<(usize, u32)>;
/// `resolve-duplicates` result: per group, member indices to keep in
/// preference order; empty = the host's `DeduplicateKeep`.
pub type PluginKeepers = Vec<Vec<usize>>;
/// `merge-groups` result: canonical group title -> member titles.
pub type PluginGroupMerge = Vec<(String, Vec<String>)>;

/// One post-merge duplicate group: the host's normalized key and the members
/// in their current order.
#[derive(Debug, Clone)]
pub struct PluginDuplicateGroup<'a> {
    pub key: &'a str,
    pub members: Vec<&'a PlaylistItem>,
}

/// Where a hook runs: the target, and the input and view when the site has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookCtx<'a> {
    pub target: &'a str,
    /// The input whose items are passed; `None` where items of several inputs
    /// are mixed (finalize-side hooks).
    pub input: Option<&'a str>,
    /// The finalized view (`base`, or the curated Xtream view) for the hooks
    /// that run once per view. `None` before views exist.
    pub view: Option<&'a str>,
}

impl<'a> HookCtx<'a> {
    #[must_use]
    pub const fn new(target: &'a str) -> Self { Self { target, input: None, view: None } }

    #[must_use]
    pub const fn with_input(self, input: &'a str) -> Self { Self { input: Some(input), ..self } }

    #[must_use]
    pub const fn with_view(self, view: &'a str) -> Self { Self { view: Some(view), ..self } }
}

/// One plugin's manifest `scope:`, compiled once at load: the items a hook of
/// that plugin is sent. Empty lists match everything.
#[derive(Debug, Clone, Default)]
pub struct PluginScope {
    /// Regexes on the group; any one matching is enough.
    groups: Vec<Regex>,
    item_types: Vec<PlaylistItemType>,
}

impl PluginScope {
    /// # Errors
    /// The first group pattern that is not a valid regex.
    pub fn new(groups: &[String], item_types: &[PlaylistItemType]) -> Result<Self, regex::Error> {
        let groups = groups.iter().map(|pattern| Regex::new(pattern)).collect::<Result<_, _>>()?;
        Ok(Self { groups, item_types: item_types.to_vec() })
    }

    /// No restriction: every item is in scope.
    #[must_use]
    pub fn is_all(&self) -> bool { self.groups.is_empty() && self.item_types.is_empty() }

    #[must_use]
    pub fn matches(&self, item: &PlaylistItem) -> bool {
        let header = &item.header;
        (self.item_types.is_empty() || self.item_types.contains(&header.item_type))
            && (self.groups.is_empty() || self.groups.iter().any(|group| group.is_match(&header.group)))
    }
}

/// The union of the scopes of every plugin that runs a hook: what a caller
/// holding a disk-backed source must materialize, and nothing more.
#[derive(Debug, Clone, Default)]
pub struct PluginScopes(pub Vec<Arc<PluginScope>>);

impl PluginScopes {
    #[must_use]
    pub fn is_all(&self) -> bool { self.0.iter().any(|scope| scope.is_all()) }

    #[must_use]
    pub fn matches(&self, item: &PlaylistItem) -> bool { self.0.iter().any(|scope| scope.matches(item)) }
}

/// What one plugin did in one hook on one target.
/// Summed over the target's inputs, so a target reports each (plugin, hook)
/// once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHookStats {
    pub plugin: String,
    /// [`PluginHook::as_str`].
    pub hook: String,
    /// Items sent to the plugin (after its scope) and not dropped by it.
    pub kept: u64,
    pub dropped: u64,
    pub modified: u64,
    /// Failed runs: trap, deadline, out of memory, `err`, no memory budget.
    /// The items of a failed run are kept unchanged.
    pub errors: u64,
    /// Changes naming a handle that was never issued, or repeating one; logged
    /// and ignored.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignored_changes: u64,
    pub duration_ms: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip_serializing_if signature")]
const fn is_zero(value: &u64) -> bool { *value == 0 }

impl PluginHookStats {
    #[must_use]
    pub fn new(plugin: &str, hook: PluginHook) -> Self {
        Self { plugin: plugin.to_string(), hook: hook.as_str().to_string(), ..Self::default() }
    }

    fn add(&mut self, other: &Self) {
        self.kept += other.kept;
        self.dropped += other.dropped;
        self.modified += other.modified;
        self.errors += other.errors;
        self.ignored_changes += other.ignored_changes;
        self.duration_ms += other.duration_ms;
    }
}

/// The identity of a plugin that ran a hook on a target. `fingerprint` digests id, version, wasm hash, the resolved
/// settings and the ABI, so a changed setting or secret changes it; the
/// settings themselves are never part of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginFingerprintStats {
    pub id: String,
    pub version: String,
    pub wasm_sha256: String,
    pub abi: String,
    pub fingerprint: String,
}

/// Everything plugins did to one target run: hook stats and the fingerprint
/// of each plugin that actually ran. Rides `PipelineStats` into the processing
/// report and the `playlist.update` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRunReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<PluginHookStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprints: Vec<PluginFingerprintStats>,
}

impl PluginRunReport {
    #[must_use]
    pub const fn is_empty(&self) -> bool { self.hooks.is_empty() && self.fingerprints.is_empty() }

    /// Add one hook run, summing into an existing (plugin, hook) entry.
    pub fn record(&mut self, stats: &PluginHookStats) {
        match self.hooks.iter_mut().find(|entry| entry.plugin == stats.plugin && entry.hook == stats.hook) {
            Some(entry) => entry.add(stats),
            None => self.hooks.push(stats.clone()),
        }
    }

    /// Note that a plugin ran; once per plugin id.
    pub fn ran(&mut self, fingerprint: PluginFingerprintStats) {
        if !self.fingerprints.iter().any(|entry| entry.id == fingerprint.id) {
            self.fingerprints.push(fingerprint);
        }
    }

    pub fn merge(&mut self, other: Self) {
        for stats in &other.hooks {
            self.record(stats);
        }
        for fingerprint in other.fingerprints {
            self.ran(fingerprint);
        }
    }
}

/// A plugin running under `fail_mode: abort-target` failed, and the target must
/// not be written.
///
/// Fail-open failures never surface here: the host logs and counts them and the
/// hook returns `Ok(())` with the items untouched.
#[derive(Debug, Clone, thiserror::Error)]
#[error("plugin {plugin} aborted target processing at {hook:?}: {reason}")]
pub struct PluginAbort {
    pub plugin: Arc<str>,
    pub hook: PluginHook,
    pub reason: String,
}

/// What the playlist pipeline needs from a plugin host.
///
/// Every method has a no-op default, so a hook added later changes
/// neither [`NoPlugins`] nor any signature the parameter is threaded through -
/// only the call site that awaits it.
pub trait PluginHost: Clone + Send + Sync + 'static {
    /// `true` when at least one enabled plugin is subscribed to `hook` for
    /// `target`. Lets the call site skip all hook work; constant `false` for
    /// [`NoPlugins`].
    fn wants(&self, _hook: PluginHook, _target: &str) -> bool { false }

    /// The union of the scopes of the plugins that run `hook` in `ctx`; `None`
    /// when none does. A caller holding a disk-backed source materializes only
    /// the items this matches.
    fn scope(&self, _hook: PluginHook, _ctx: &HookCtx<'_>) -> Option<PluginScopes> { None }

    /// Run a stage hook over the items of one input of a target.
    ///
    /// The host may drop items and rewrite their fields. It never adds items or
    /// changes host-owned fields (virtual id, item type, cluster, input name,
    /// input stream id). Callers re-normalize grouping afterwards, since a
    /// plugin may have rewritten `group`. What each plugin did is added to
    /// `report`.
    fn on_stage(
        &self,
        _hook: PluginHook,
        _ctx: &HookCtx<'_>,
        _items: &mut Vec<PlaylistItem>,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<(), PluginAbort>> + Send {
        async { Ok(()) }
    }

    /// Run `on-playlist-loaded` over `items`, which must be only the items of
    /// the input that [`PluginHost::scope`] matched. Same rules as
    /// [`PluginHost::on_stage`]; `ctx.target` is empty, `ctx.input` is set.
    fn on_playlist_loaded(
        &self,
        _ctx: &HookCtx<'_>,
        _items: &mut Vec<PlaylistItem>,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<(), PluginAbort>> + Send {
        async { Ok(()) }
    }

    /// Return managed channels for an input, without modifying provider items.
    /// The host reconciles each owner's snapshot and retains last-good data on failures.
    fn managed_channels(
        &self,
        _plugin: &str,
        _request: &ManagedChannelRequest<'_>,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Vec<PlaylistItem>, PluginAbort>> + Send {
        async { Ok(Vec::new()) }
    }

    /// `true` when `plugin` is enabled and runs `hook`. Strategy and filter
    /// references name one plugin; this is how config validation and the call
    /// sites check them.
    fn provides(&self, _hook: PluginHook, _plugin: &str) -> bool { false }

    /// Combined fingerprint of the plugins that run `hook`, for host-side
    /// caches of plugin-processed data; `None` when
    /// none does.
    fn fingerprint(&self, _hook: PluginHook) -> Option<String> { None }

    /// `after-sort` over one finalized view, flattened in view order. Returns
    /// the new order as a permutation of `0..items.len()`, or `None` to keep
    /// the order. Runs every plugin that exports the hook, chained.
    fn after_sort(
        &self,
        _ctx: &HookCtx<'_>,
        _items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Option<Vec<usize>>, PluginAbort>> + Send {
        async { Ok(None) }
    }

    /// `sort-key` of `plugin`: one key per item, index-aligned; `None` means
    /// the host default (plugin missing or failed fail-open).
    fn sort_keys(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Option<PluginSortKeys>, PluginAbort>> + Send {
        async { Ok(None) }
    }

    /// `assign-channel-numbers` of `plugin`: `(index into items, chno)` pairs.
    /// Unlisted items keep host numbering; chno 0 is ignored.
    fn assign_chno(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Option<PluginChnoMap>, PluginAbort>> + Send {
        async { Ok(None) }
    }

    /// `resolve-duplicates` of `plugin`: per group (aligned with `groups`) the
    /// member indices to keep, in preference order; an empty list means the
    /// host's `DeduplicateKeep` for that group.
    fn resolve_duplicates(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _groups: &[PluginDuplicateGroup<'_>],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Option<PluginKeepers>, PluginAbort>> + Send {
        async { Ok(None) }
    }

    /// `merge-groups` of `plugin`: canonical group title -> member titles.
    fn merge_groups(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _groups: &[String],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<Option<PluginGroupMerge>, PluginAbort>> + Send {
        async { Ok(None) }
    }

    /// `eval-filter-batch` for one `plugin.<id>.<fn>(args)` filter term over
    /// `items`: index-aligned decisions. Never aborts: a failure (or a missing
    /// plugin) is all `false`, counted in `report`.
    fn eval_filter_batch(
        &self,
        _plugin: &str,
        _func: &str,
        _args: &[String],
        _ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Vec<bool>> + Send {
        let decisions = vec![false; items.len()];
        async move { decisions }
    }

    /// `on-epg-loaded` over one EPG source's channels (`ctx.input` = source).
    /// Sparse: a returned id replaces that channel's names and icon. EPG and
    /// metadata hooks are always fail-open.
    fn on_epg_loaded(
        &self,
        _ctx: &HookCtx<'_>,
        _channels: &mut Vec<PluginEpgChannel>,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// `transform-programmes` over one bounded batch of a source. The batch is
    /// replaced by what the plugins return (drop, rewrite, add); the host
    /// re-attaches the transient channel id.
    fn transform_programmes(
        &self,
        _ctx: &HookCtx<'_>,
        _programmes: &mut Vec<EpgProgramme>,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// `enrich-programmes` over the channels of one input: one instance per
    /// plugin, one guest call per channel.
    fn enrich_programmes(
        &self,
        _ctx: &HookCtx<'_>,
        _channels: &mut [PluginChannelProgrammes],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// `match-channel` second pass: per request (index-aligned), the winning
    /// candidate id or `None`.
    fn match_channels(
        &self,
        _ctx: &HookCtx<'_>,
        requests: &[PluginMatchRequest<'_>],
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Vec<Option<String>>> + Send {
        let decisions = vec![None; requests.len()];
        async move { decisions }
    }

    /// `enrich-metadata` over one resolved VOD / series item: the amended
    /// metadata JSON, or `None` when no plugin changed it.
    fn enrich_metadata(
        &self,
        _ctx: &HookCtx<'_>,
        _item: &PlaylistItem,
        _metadata_json: &str,
        _report: &mut PluginRunReport,
    ) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }

    /// `fetch-playlist` of the plugin a `type: plugin` input names: the items
    /// enter the pipeline like a downloaded playlist. `Err` fails the input
    /// download exactly like an unreachable provider.
    fn fetch_playlist(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _request: &PluginInputRequest<'_>,
    ) -> impl Future<Output = Result<Vec<PlaylistItem>, String>> + Send {
        async { Err(NO_PLUGIN_HOST.to_string()) }
    }

    /// `fetch-epg` of the plugin a `type: plugin` input names; `Ok(vec![])`
    /// when the provider has no EPG.
    fn fetch_epg(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _request: &PluginInputRequest<'_>,
    ) -> impl Future<Output = Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String>> + Send {
        async { Err(NO_PLUGIN_HOST.to_string()) }
    }

    /// The `provides.output_format` of `plugin`, when it is enabled and renders
    /// outputs; the post-load check compares a target output's `format` with it.
    fn output_format(&self, _plugin: &str) -> Option<String> { None }

    /// `begin-output -> render-batch… -> finish-output` of one `type: plugin`
    /// target output, written atomically to `request.path`. `Err` fails only
    /// this output; the caller logs it and carries on with the others.
    fn render_output(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _request: &PluginOutputRequest<'_>,
        _items: &[&PlaylistItem],
    ) -> impl Future<Output = Result<(), String>> + Send {
        async { Err(NO_PLUGIN_HOST.to_string()) }
    }
}

/// The `Err` of a provider call when no plugin host is running.
pub const NO_PLUGIN_HOST: &str = "plugins are not enabled";

/// The plugin host of a run with no plugins: the feature is off, plugins are
/// disabled, or nothing is installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPlugins;

impl PluginHost for NoPlugins {}

/// A host chosen at runtime: `None` when `plugins.enabled` is off.
///
/// Costs one branch in `wants` per hook site per input; the hook itself only
/// runs when that returns `true`.
impl<P: PluginHost> PluginHost for Option<P> {
    fn wants(&self, hook: PluginHook, target: &str) -> bool {
        self.as_ref().is_some_and(|host| host.wants(hook, target))
    }

    fn scope(&self, hook: PluginHook, ctx: &HookCtx<'_>) -> Option<PluginScopes> {
        self.as_ref().and_then(|host| host.scope(hook, ctx))
    }

    async fn on_stage(
        &self,
        hook: PluginHook,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        match self {
            Some(host) => host.on_stage(hook, ctx, items, report).await,
            None => Ok(()),
        }
    }

    async fn on_playlist_loaded(
        &self,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        match self {
            Some(host) => host.on_playlist_loaded(ctx, items, report).await,
            None => Ok(()),
        }
    }

    async fn managed_channels(
        &self,
        plugin: &str,
        request: &ManagedChannelRequest<'_>,
        report: &mut PluginRunReport,
    ) -> Result<Vec<PlaylistItem>, PluginAbort> {
        match self {
            Some(host) => host.managed_channels(plugin, request, report).await,
            None => Ok(Vec::new()),
        }
    }

    fn provides(&self, hook: PluginHook, plugin: &str) -> bool {
        self.as_ref().is_some_and(|host| host.provides(hook, plugin))
    }

    fn fingerprint(&self, hook: PluginHook) -> Option<String> { self.as_ref().and_then(|host| host.fingerprint(hook)) }

    async fn after_sort(
        &self,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<Vec<usize>>, PluginAbort> {
        match self {
            Some(host) => host.after_sort(ctx, items, report).await,
            None => Ok(None),
        }
    }

    async fn sort_keys(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginSortKeys>, PluginAbort> {
        match self {
            Some(host) => host.sort_keys(plugin, ctx, items, report).await,
            None => Ok(None),
        }
    }

    async fn assign_chno(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginChnoMap>, PluginAbort> {
        match self {
            Some(host) => host.assign_chno(plugin, ctx, items, report).await,
            None => Ok(None),
        }
    }

    async fn resolve_duplicates(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        groups: &[PluginDuplicateGroup<'_>],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginKeepers>, PluginAbort> {
        match self {
            Some(host) => host.resolve_duplicates(plugin, ctx, groups, report).await,
            None => Ok(None),
        }
    }

    async fn merge_groups(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        groups: &[String],
        report: &mut PluginRunReport,
    ) -> Result<Option<PluginGroupMerge>, PluginAbort> {
        match self {
            Some(host) => host.merge_groups(plugin, ctx, groups, report).await,
            None => Ok(None),
        }
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
        match self {
            Some(host) => host.eval_filter_batch(plugin, func, args, ctx, items, report).await,
            None => vec![false; items.len()],
        }
    }

    async fn on_epg_loaded(
        &self,
        ctx: &HookCtx<'_>,
        channels: &mut Vec<PluginEpgChannel>,
        report: &mut PluginRunReport,
    ) {
        if let Some(host) = self {
            host.on_epg_loaded(ctx, channels, report).await;
        }
    }

    async fn transform_programmes(
        &self,
        ctx: &HookCtx<'_>,
        programmes: &mut Vec<EpgProgramme>,
        report: &mut PluginRunReport,
    ) {
        if let Some(host) = self {
            host.transform_programmes(ctx, programmes, report).await;
        }
    }

    async fn enrich_programmes(
        &self,
        ctx: &HookCtx<'_>,
        channels: &mut [PluginChannelProgrammes],
        report: &mut PluginRunReport,
    ) {
        if let Some(host) = self {
            host.enrich_programmes(ctx, channels, report).await;
        }
    }

    async fn match_channels(
        &self,
        ctx: &HookCtx<'_>,
        requests: &[PluginMatchRequest<'_>],
        report: &mut PluginRunReport,
    ) -> Vec<Option<String>> {
        match self {
            Some(host) => host.match_channels(ctx, requests, report).await,
            None => vec![None; requests.len()],
        }
    }

    async fn enrich_metadata(
        &self,
        ctx: &HookCtx<'_>,
        item: &PlaylistItem,
        metadata_json: &str,
        report: &mut PluginRunReport,
    ) -> Option<String> {
        match self {
            Some(host) => host.enrich_metadata(ctx, item, metadata_json, report).await,
            None => None,
        }
    }

    async fn fetch_playlist(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginInputRequest<'_>,
    ) -> Result<Vec<PlaylistItem>, String> {
        match self {
            Some(host) => host.fetch_playlist(plugin, ctx, request).await,
            None => Err(NO_PLUGIN_HOST.to_string()),
        }
    }

    async fn fetch_epg(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginInputRequest<'_>,
    ) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String> {
        match self {
            Some(host) => host.fetch_epg(plugin, ctx, request).await,
            None => Err(NO_PLUGIN_HOST.to_string()),
        }
    }

    fn output_format(&self, plugin: &str) -> Option<String> {
        self.as_ref().and_then(|host| host.output_format(plugin))
    }

    async fn render_output(
        &self,
        plugin: &str,
        ctx: &HookCtx<'_>,
        request: &PluginOutputRequest<'_>,
        items: &[&PlaylistItem],
    ) -> Result<(), String> {
        match self {
            Some(host) => host.render_output(plugin, ctx, request, items).await,
            None => Err(NO_PLUGIN_HOST.to_string()),
        }
    }
}

/// What `inject-upstream-headers` sees of one upstream stream request
/// (`stream-hooks.upstream-request`).
#[derive(Debug, Clone, Copy)]
pub struct PluginUpstreamRequest<'a> {
    /// The target the stream URL belongs to; with `virtual_id` and `user` the
    /// key of the host's header cache.
    pub target_id: u16,
    /// The persisted `VirtualId` from the stream URL.
    pub virtual_id: u32,
    pub url: &'a str,
    /// The headers the request carries so far.
    pub headers: &'a [(String, String)],
    /// The requesting api-user name.
    pub user: &'a str,
}

/// The session-edge header injection interface:
/// the stream proxy asks it once per stream-session setup, before the
/// upstream request is built.
///
/// Fail-open by contract: a slow, trapping or unapproved plugin yields no
/// headers and the stream starts anyway.
pub trait PluginHeaderInjector: Clone + Send + Sync + 'static {
    /// `false` when no enabled plugin injects headers, so the proxy skips the
    /// call; constant `false` for [`NoPlugins`].
    fn wants_upstream_headers(&self) -> bool { false }

    /// The extra headers to add, already denylist-filtered.
    fn inject_upstream_headers(
        &self,
        _request: &PluginUpstreamRequest<'_>,
    ) -> impl Future<Output = Vec<(String, String)>> + Send {
        async { Vec::new() }
    }
}

impl PluginHeaderInjector for NoPlugins {}

impl<P: PluginHeaderInjector> PluginHeaderInjector for Option<P> {
    fn wants_upstream_headers(&self) -> bool { self.as_ref().is_some_and(PluginHeaderInjector::wants_upstream_headers) }

    async fn inject_upstream_headers(&self, request: &PluginUpstreamRequest<'_>) -> Vec<(String, String)> {
        match self {
            Some(host) => host.inject_upstream_headers(request).await,
            None => Vec::new(),
        }
    }
}

/// `notification-hooks.severity` of a delivered notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSeverity {
    Info,
    Warn,
    Error,
}

/// One notification handed to a plugin channel (`notification-hooks.notification`).
#[derive(Debug, Clone, Copy)]
pub struct PluginNotification<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub severity: PluginSeverity,
    /// The raw event JSON, for rich formatting.
    pub payload: &'a str,
}

/// The outcome of a plugin delivery, 1:1 with messaging's `Delivery`
/// (`notification-hooks.delivery-error`). A trap or timeout is `Retry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginDelivery {
    Delivered,
    Retry { reason: String, after: Option<Duration> },
    Permanent { reason: String },
}

/// A loaded plugin that delivers notifications: one `Channel::Plugin`.
#[derive(Debug, Clone)]
pub struct PluginNotificationChannel {
    /// The stable wire id of the channel (outbox key): the plugin id.
    pub plugin: Arc<str>,
    /// The `routing` object of the plugin's settings, parsed by messaging
    /// like a built-in channel's routing; `None` means the defaults.
    pub routing: Option<serde_json::Value>,
}

/// A boxed delivery: the notifier is reached through a trait object, so its
/// future is boxed (one allocation per notification, off the hot path).
pub type PluginDeliveryFuture<'a> = Pin<Box<dyn Future<Output = PluginDelivery> + Send + 'a>>;

/// The messaging interface. Object safe:
/// messaging holds an `Arc<dyn PluginNotifier>` registered by `app`, so the
/// closed `Channel` enum takes no generic parameter.
pub trait PluginNotifier: Send + Sync + 'static {
    /// The plugin channels right now, in a stable order.
    fn notification_channels(&self) -> Vec<PluginNotificationChannel>;

    /// Deliver through `plugin` under the call deadline. Never blocks the
    /// caller beyond that deadline; never panics.
    fn send_notification<'a>(&'a self, plugin: &'a str, message: PluginNotification<'a>) -> PluginDeliveryFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::plugin_options;

    #[test]
    fn plugin_options_flatten_nested_values_and_skip_nulls() {
        let config = serde_json::json!({
            "mode": "live",
            "limit": 5,
            "unset": null,
            "nested": {"a": true, "b": null, "empty": {}},
        });
        let mut options = plugin_options(config.as_object().unwrap());
        options.sort();
        assert_eq!(
            options,
            vec![
                ("limit".to_string(), "5".to_string()),
                ("mode".to_string(), "live".to_string()),
                ("nested.a".to_string(), "true".to_string()),
            ]
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedDestination {
    Input,
    Target,
}
#[derive(Debug, Clone, Copy)]
pub struct ManagedChannelRequest<'a> {
    pub destination: ManagedDestination,
    pub name: &'a str,
    pub stage: Option<super::ManagedChannelStage>,
    pub inputs: &'a [String],
}
