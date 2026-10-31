//! The hook dispatcher (plan §3 *Key structural decisions*, §7 *Dispatcher*,
//! §18 *W4 seam freeze*).
//!
//! One hook call runs every enabled plugin that exports the hook, in
//! (priority, id) order, each over the items the one before it left: plugin B
//! receives A's output. Per plugin:
//!
//! 1. **Scope.** Only the items its manifest `scope:` matches are sent. A
//!    plugin whose scope matches nothing is never instantiated.
//! 2. **Run.** One fresh instance, which takes its memory-budget permits and an
//!    instance slot and returns them when dropped. Then `init`, then the scoped
//!    items in batches of `plugins.batch_size`, each batch issuing handles
//!    `0..len`.
//! 3. **Merge,** only once every batch succeeded. The merge is sparse:
//!    `drop(handle)` removes an item, `modified(channel)` rewrites its writable
//!    fields, and an item nobody mentions is kept. A handle never issued, or
//!    one already answered, is ignored and counted; the first entry wins.
//! 4. **Failure.** A trap, deadline, out-of-memory, `err` or lack of budget
//!    leaves the items exactly as the previous plugin left them. It is counted
//!    on the plugin's circuit breaker, and then either the next plugin runs
//!    (fail-open) or the target is aborted (`fail_mode: abort-target`).
//!
//! The dispatcher only ever sees a `Vec` of items. The pipeline owns the
//! `TransformBuffer` (and regroups after a `group` change) and the disk-backed
//! `PlaylistSource` (and materializes only what [`HookDispatcher::scope`]
//! matches).

use crate::{
    convert::{apply_channel, item_to_channel},
    host_fns,
    manifest::FailMode,
    registry::{sha256_hex, LoadedPlugin, PluginRecord, RegistrySnapshot},
    runtime::{call_with_deadline, CallError, LiveInstance, PreparedPlugin, Runtime},
};
use shared::model::{HookCtx, PlaylistItem, PluginAbort, PluginHook, PluginHookStats, PluginRunReport, PluginScopes};
use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::{playlist_hooks, source_hooks},
    tuliprox::plugin::types::{Channel, ChannelChange},
};

/// Which guest function a hook calls.
#[derive(Clone, Copy)]
enum Call {
    Stage(playlist_hooks::Stage),
    Loaded,
}

/// The `playlist-hooks` stage a stage hook calls; `None` for every other hook.
const fn stage_for(hook: PluginHook) -> Option<playlist_hooks::Stage> {
    match hook {
        PluginHook::BeforeFilter => Some(playlist_hooks::Stage::BeforeFilter),
        PluginHook::AfterFilter => Some(playlist_hooks::Stage::AfterFilter),
        PluginHook::BeforeRename => Some(playlist_hooks::Stage::BeforeRename),
        PluginHook::AfterRename => Some(playlist_hooks::Stage::AfterRename),
        PluginHook::BeforeMap => Some(playlist_hooks::Stage::BeforeMap),
        PluginHook::AfterMap => Some(playlist_hooks::Stage::AfterMap),
        _ => None,
    }
}

/// The loaded export a [`Call`] goes through.
enum Guest {
    Stage(playlist_hooks::Stage, playlist_hooks::Guest),
    Loaded(source_hooks::Guest),
}

/// Dispatches over one registry snapshot, so a reload mid-run never mixes two
/// plugin sets in one hook call.
///
/// The W5 hook families (`strategy.rs`, `filter_batch.rs`, `epg_hooks.rs`)
/// build on the same pieces as [`HookDispatcher::run`]: [`Self::runnable`] /
/// [`Self::named`] to pick plugins, [`Self::open`] for a fresh initialized
/// instance, then per guest call
/// `call_with_deadline(live.limits(), guest.call_x(live.armed(), …)).await`
/// followed by `host_fns::end_call(live.store_mut().data_mut()).await`, and
/// finally `HookDispatcher::settle` once per plugin.
pub(crate) struct HookDispatcher<'a> {
    runtime: &'a Runtime,
    snapshot: Arc<RegistrySnapshot>,
}

impl<'a> HookDispatcher<'a> {
    pub(crate) const fn new(runtime: &'a Runtime, snapshot: Arc<RegistrySnapshot>) -> Self {
        Self { runtime, snapshot }
    }

    /// The runtime short-circuit: `false` when no enabled plugin exports `hook`.
    pub(crate) fn wants(&self, hook: PluginHook) -> bool { self.snapshot.runnable(hook).next().is_some() }

    /// `plugins.batch_size`: items per guest call, at least 1.
    pub(crate) fn batch_size(&self) -> usize { self.snapshot.batch_size.max(1) }

    /// The plugins that run `hook`, in run order, with their loaded state.
    pub(crate) fn runnable(&self, hook: PluginHook) -> impl Iterator<Item = (&Arc<PluginRecord>, &LoadedPlugin)> {
        self.snapshot.runnable(hook).filter_map(|plugin| plugin.loaded.as_ref().map(|loaded| (plugin, loaded)))
    }

    /// The plugin `id`, if it is enabled and runs `hook`. Strategy and filter
    /// references name exactly one plugin.
    pub(crate) fn named(&self, hook: PluginHook, id: &str) -> Option<(&Arc<PluginRecord>, &LoadedPlugin)> {
        self.runnable(hook).find(|(plugin, _)| &*plugin.id == id)
    }

    /// The combined fingerprint of the plugins that run `hook`: the single
    /// plugin's digest, or a digest over every (id, digest) in run order.
    pub(crate) fn fingerprint(&self, hook: PluginHook) -> Option<String> {
        let digests: Vec<_> = self.runnable(hook).map(|(_, loaded)| loaded.current_fingerprint()).collect();
        match digests.as_slice() {
            [] => None,
            [only] => Some(only.digest.clone()),
            many => {
                let joined = many.iter().fold(String::new(), |mut joined, print| {
                    let _ = writeln!(joined, "{}\n{}", print.id, print.digest);
                    joined
                });
                Some(sha256_hex(joined.as_bytes()))
            }
        }
    }

    /// The union of the scopes of the plugins that run `hook`.
    pub(crate) fn scope(&self, hook: PluginHook) -> Option<PluginScopes> {
        let scopes: Vec<_> = self.runnable(hook).map(|(_, loaded)| Arc::clone(&loaded.scope)).collect();
        (!scopes.is_empty()).then_some(PluginScopes(scopes))
    }

    /// Run a stage hook over the items of one input of a target.
    pub(crate) async fn run_playlist_hook(
        &self,
        hook: PluginHook,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        let Some(stage) = stage_for(hook) else {
            debug_assert!(false, "{} is not a stage hook", hook.as_str());
            return Ok(());
        };
        self.run(hook, Call::Stage(stage), ctx, items, report).await
    }

    /// Run `on-playlist-loaded` over the scoped subset of one input.
    pub(crate) async fn run_playlist_loaded(
        &self,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        self.run(PluginHook::PlaylistLoaded, Call::Loaded, ctx, items, report).await
    }

    /// A fresh instance of `loaded` with `init` already run: memory-budget
    /// permits and an instance slot are held until it is dropped.
    pub(crate) async fn open(&self, loaded: &LoadedPlugin) -> Result<LiveInstance, CallError> {
        self.open_prepared(loaded, &loaded.prepared).await
    }

    /// [`Self::open`] under another call deadline: a task run, a provider call
    /// ([`crate::runtime::PluginLimits::provider_exec`]) or the header clamp.
    /// The deadline also bounds the wait for budget and the `init` call.
    pub(crate) async fn open_with(&self, loaded: &LoadedPlugin, exec: Duration) -> Result<LiveInstance, CallError> {
        self.open_prepared(loaded, &loaded.prepared.with_exec(exec)).await
    }

    /// The runtime the snapshot runs on (its clamps: `config()`).
    pub(crate) const fn runtime(&self) -> &Runtime { self.runtime }

    async fn open_prepared(&self, loaded: &LoadedPlugin, prepared: &PreparedPlugin) -> Result<LiveInstance, CallError> {
        let mut live = self.runtime.instantiate(prepared, &loaded.env).await?;
        let instance = live.instance();
        let lifecycle =
            loaded.lifecycle.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
        let limits = live.limits();
        let init = call_with_deadline(limits, lifecycle.call_init(live.armed(), &loaded.env.settings)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        init?.map_err(|err| CallError::Trap(format!("init returned err: {err}")))?;
        Ok(live)
    }

    /// Close one plugin's run of `hook`: the circuit breaker, the per-plugin
    /// and per-target stats, the fingerprint (on success), the log line, and
    /// the fail-mode decision. Fill `stats.kept` / `dropped` / `modified` /
    /// `ignored_changes` before calling; `errors` and `duration_ms` are set
    /// here. Returns the abort for `fail_mode: abort-target`; fail-open-only
    /// hooks (filter, EPG, metadata) ignore it.
    #[allow(clippy::too_many_arguments, reason = "one call per plugin run, every argument is needed")]
    pub(crate) fn settle(
        plugin: &PluginRecord,
        loaded: &LoadedPlugin,
        hook: PluginHook,
        ctx: &HookCtx<'_>,
        mut stats: PluginHookStats,
        started: Instant,
        outcome: Result<(), &CallError>,
        report: &mut PluginRunReport,
    ) -> Option<PluginAbort> {
        plugin.report(outcome);
        let abort = match outcome {
            Ok(()) => {
                report.ran(loaded.current_fingerprint().stats());
                log::debug!(
                    "[plugin:{}] {} on `{}`: {} kept, {} modified, {} dropped",
                    plugin.id,
                    hook.as_str(),
                    ctx.target,
                    stats.kept,
                    stats.modified,
                    stats.dropped
                );
                None
            }
            Err(err) => {
                stats.errors = 1;
                let abort = loaded.fail_mode == FailMode::AbortTarget;
                log::error!(
                    "[plugin:{}] {} on `{}` failed, {}: {err}",
                    plugin.id,
                    hook.as_str(),
                    ctx.target,
                    if abort { "aborting the target (fail_mode: abort-target)" } else { "host default kept" }
                );
                abort.then(|| PluginAbort { plugin: Arc::clone(&plugin.id), hook, reason: err.to_string() })
            }
        };
        stats.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        plugin.record_hook(&stats);
        report.record(&stats);
        abort
    }

    async fn run(
        &self,
        hook: PluginHook,
        call: Call,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        let batch_size = self.batch_size();
        for (plugin, loaded) in self.runnable(hook) {
            let scoped = scoped_indices(loaded, items);
            if scoped.is_empty() {
                continue;
            }
            let started = Instant::now();
            let mut stats = PluginHookStats::new(&plugin.id, hook);
            let outcome = self.run_plugin(loaded, call, ctx, items, &scoped, batch_size).await;
            let outcome = match outcome {
                Ok(results) => {
                    let merged = merge(&plugin.id, items, &scoped, batch_size, results);
                    stats.kept = count(scoped.len() - merged.dropped);
                    stats.dropped = count(merged.dropped);
                    stats.modified = count(merged.modified);
                    stats.ignored_changes = count(merged.ignored);
                    Ok(())
                }
                Err(err) => {
                    stats.kept = count(scoped.len());
                    Err(err)
                }
            };
            if let Some(abort) =
                Self::settle(plugin, loaded, hook, ctx, stats, started, outcome.as_ref().copied(), report)
            {
                return Err(abort);
            }
        }
        Ok(())
    }

    /// One plugin over the scoped items, all in one fresh instance. All or
    /// nothing: the first failing call ends the run and nothing is applied.
    async fn run_plugin(
        &self,
        loaded: &LoadedPlugin,
        call: Call,
        ctx: &HookCtx<'_>,
        items: &[PlaylistItem],
        scoped: &[usize],
        batch_size: usize,
    ) -> Result<Vec<Vec<ChannelChange>>, CallError> {
        let mut live = self.open(loaded).await?;
        let instance = live.instance();

        let missing = |interface: &str| CallError::Instantiate(format!("does not export {interface}"));
        let bind = |err: wasmtime::Error| CallError::Trap(format!("{err:#}"));
        let guest = match call {
            Call::Stage(stage) => {
                let indices = loaded.playlist_hooks.as_ref().ok_or_else(|| missing("playlist-hooks"))?;
                Guest::Stage(stage, indices.load(live.store_mut(), &instance).map_err(bind)?)
            }
            Call::Loaded => {
                let indices = loaded.source_hooks.as_ref().ok_or_else(|| missing("source-hooks"))?;
                Guest::Loaded(indices.load(live.store_mut(), &instance).map_err(bind)?)
            }
        };

        let mut results = Vec::with_capacity(scoped.len().div_ceil(batch_size));
        for chunk in scoped.chunks(batch_size) {
            let channels: Vec<Channel> = chunk
                .iter()
                .enumerate()
                // A batch never exceeds MAX_PLUGINS_BATCH_SIZE, which fits in u32.
                .map(|(handle, &index)| item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), &items[index]))
                .collect();
            let limits = live.limits();
            let changes = match &guest {
                Guest::Stage(stage, hooks) => {
                    call_with_deadline(limits, hooks.call_on_stage(live.armed(), *stage, ctx.target, &channels)).await
                }
                Guest::Loaded(hooks) => {
                    let input = ctx.input.unwrap_or_default();
                    call_with_deadline(limits, hooks.call_on_playlist_loaded(live.armed(), input, &channels)).await
                }
            };
            host_fns::end_call(live.store_mut().data_mut()).await;
            results.push(changes?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?);
        }
        Ok(results)
    }
}

pub(crate) fn count(value: usize) -> u64 { u64::try_from(value).unwrap_or(u64::MAX) }

/// Indices into `items` of what `loaded`'s scope matches, in order.
fn scoped_indices(loaded: &LoadedPlugin, items: &[PlaylistItem]) -> Vec<usize> {
    if loaded.scope.is_all() {
        return (0..items.len()).collect();
    }
    items.iter().enumerate().filter(|(_, item)| loaded.scope.matches(item)).map(|(index, _)| index).collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MergeStats {
    dropped: usize,
    /// `modified` entries that changed at least one writable field.
    modified: usize,
    /// Unknown or repeated handles.
    ignored: usize,
}

/// Apply one plugin's results to `items`.
///
/// `results[b]` answers batch `b`, which carried `scoped[b * batch_size..]` (at
/// most `batch_size` of them) under handles `0..len`.
fn merge(
    plugin: &str,
    items: &mut Vec<PlaylistItem>,
    scoped: &[usize],
    batch_size: usize,
    results: Vec<Vec<ChannelChange>>,
) -> MergeStats {
    let mut stats = MergeStats::default();
    let (mut unknown, mut repeated) = (0usize, 0usize);
    let mut answered = vec![false; scoped.len()];
    let mut dropped = vec![false; items.len()];

    for (batch, changes) in results.into_iter().enumerate() {
        let start = batch * batch_size;
        let len = scoped.len().saturating_sub(start).min(batch_size);
        for change in changes {
            let (handle, modified) = match change {
                ChannelChange::Drop(handle) => (handle, None),
                ChannelChange::Modified(channel) => (channel.handle, Some(channel)),
            };
            let Some(slot) = usize::try_from(handle).ok().filter(|handle| *handle < len).map(|handle| start + handle)
            else {
                unknown += 1;
                continue;
            };
            if std::mem::replace(&mut answered[slot], true) {
                repeated += 1;
                continue;
            }
            let index = scoped[slot];
            if let Some(channel) = modified {
                stats.modified += usize::from(apply_channel(&mut items[index], channel));
            } else {
                dropped[index] = true;
                stats.dropped += 1;
            }
        }
    }

    stats.ignored = unknown + repeated;
    if stats.ignored > 0 {
        log::warn!(
            "[plugin:{plugin}] ignored {unknown} changes for unknown handles and {repeated} repeated ones (the first \
             change per handle wins)"
        );
    }
    if stats.dropped > 0 {
        let mut index = 0;
        items.retain(|_| {
            let keep = !dropped[index];
            index += 1;
            keep
        });
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{model::PlaylistItemHeader, utils::Internable};

    fn item(name: &str, group: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: group.intern(),
                url: format!("http://host/{name}").intern(),
                input_name: "input".intern(),
                input_stream_id: format!("id-{name}").intern(),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn items(names: &[&str]) -> Vec<PlaylistItem> { names.iter().map(|name| item(name, "News")).collect() }

    fn names(items: &[PlaylistItem]) -> Vec<String> { items.iter().map(|i| i.header.name.to_string()).collect() }

    /// The channel the plugin was sent for `items[index]` under `handle`, with
    /// its group rewritten.
    fn regrouped(items: &[PlaylistItem], index: usize, handle: u32, group: &str) -> ChannelChange {
        let mut channel = item_to_channel(handle, &items[index]);
        channel.group = group.to_string();
        ChannelChange::Modified(channel)
    }

    fn all(list: &[PlaylistItem]) -> Vec<usize> { (0..list.len()).collect() }

    #[test]
    fn no_changes_keep_everything() {
        let mut list = items(&["a", "b", "c"]);
        let scoped = all(&list);
        assert_eq!(merge("p", &mut list, &scoped, 500, vec![vec![]]), MergeStats::default());
        assert_eq!(names(&list), ["a", "b", "c"]);
    }

    #[test]
    fn drop_and_modified_apply_by_handle_and_keep_order() {
        let mut list = items(&["a", "b", "c", "d"]);
        let scoped = all(&list);
        let changes = vec![ChannelChange::Drop(0), regrouped(&list, 2, 2, "Sport"), ChannelChange::Drop(3)];
        let stats = merge("p", &mut list, &scoped, 500, vec![changes]);
        assert_eq!(stats, MergeStats { dropped: 2, modified: 1, ignored: 0 });
        assert_eq!(names(&list), ["b", "c"]);
        assert_eq!(&*list[1].header.group, "Sport");
    }

    #[test]
    fn a_modified_entry_that_changes_nothing_is_not_counted() {
        let mut list = items(&["a"]);
        let scoped = all(&list);
        let same = ChannelChange::Modified(item_to_channel(0, &list[0]));
        assert_eq!(merge("p", &mut list, &scoped, 500, vec![vec![same]]).modified, 0);
    }

    #[test]
    fn unknown_and_repeated_handles_are_ignored_and_the_first_entry_wins() {
        let mut list = items(&["a", "b"]);
        let scoped = all(&list);
        let changes = vec![
            regrouped(&list, 1, 1, "First"),
            ChannelChange::Drop(1),
            ChannelChange::Drop(2),
            ChannelChange::Drop(u32::MAX),
        ];
        let stats = merge("p", &mut list, &scoped, 500, vec![changes]);
        assert_eq!(stats, MergeStats { dropped: 0, modified: 1, ignored: 3 });
        assert_eq!(names(&list), ["a", "b"]);
        assert_eq!(&*list[1].header.group, "First");
    }

    #[test]
    fn handles_are_relative_to_their_batch() {
        let mut list = items(&["a", "b", "c", "d", "e"]);
        let scoped = all(&list);
        // batch 0 = a,b; batch 1 = c,d; batch 2 = e
        let results = vec![
            vec![ChannelChange::Drop(1), ChannelChange::Drop(2)], // 2 is out of range for a batch of 2
            vec![ChannelChange::Drop(0)],
            vec![ChannelChange::Drop(0), ChannelChange::Drop(1)], // the last batch has one item
        ];
        let stats = merge("p", &mut list, &scoped, 2, results);
        assert_eq!(stats, MergeStats { dropped: 3, modified: 0, ignored: 2 });
        assert_eq!(names(&list), ["a", "d"]);
    }

    #[test]
    fn handles_address_the_scoped_subset_only() {
        let mut list = items(&["a", "b", "c", "d"]);
        // the plugin was sent b and d only, as handles 0 and 1
        let scoped = vec![1, 3];
        let results = vec![vec![ChannelChange::Drop(1), regrouped(&list, 1, 0, "Sport"), ChannelChange::Drop(2)]];
        let stats = merge("p", &mut list, &scoped, 500, results);
        assert_eq!(stats, MergeStats { dropped: 1, modified: 1, ignored: 1 });
        assert_eq!(names(&list), ["a", "b", "c"]);
        assert_eq!(&*list[1].header.group, "Sport");
        assert_eq!(&*list[0].header.group, "News", "unscoped items are untouched");
        assert_eq!(&*list[2].header.group, "News", "unscoped items are untouched");
    }

    #[test]
    fn host_owned_fields_never_change_through_a_merge() {
        let mut list = items(&["a"]);
        let scoped = all(&list);
        let mut channel = item_to_channel(0, &list[0]);
        channel.input_name = "hijacked".to_string();
        channel.input_stream_id = "hijacked".to_string();
        merge("p", &mut list, &scoped, 500, vec![vec![ChannelChange::Modified(channel)]]);
        assert_eq!(&*list[0].header.input_name, "input");
        assert_eq!(&*list[0].header.input_stream_id, "id-a");
    }

    #[test]
    fn hooks_map_to_their_guest_calls() {
        assert!(matches!(stage_for(PluginHook::AfterFilter), Some(playlist_hooks::Stage::AfterFilter)));
        assert!(matches!(stage_for(PluginHook::BeforeMap), Some(playlist_hooks::Stage::BeforeMap)));
        assert!(stage_for(PluginHook::PlaylistLoaded).is_none());
        assert!(stage_for(PluginHook::AfterSort).is_none());
    }
}
