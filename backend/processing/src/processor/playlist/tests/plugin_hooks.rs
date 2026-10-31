#![allow(clippy::unused_async_trait_impl, reason = "the PluginHost methods are async fns without awaits in this spy")]
use super::{
    execute_pipeline_on_groups, execute_pipeline_on_items, prepare_eligible_catalog, target::finalize_playlist_view,
};
use crate::processor::playlist::ingest::apply_playlist_loaded_hook;
use shared::{
    foundation::get_filter,
    model::{
        ConfigTargetDto, DeduplicateConfig, DeduplicateKeep, DeduplicateMatchBy, HookCtx, NoPlugins, PlaylistGroup,
        PlaylistItem, PlaylistItemHeader, PluginAbort, PluginChnoMap, PluginDuplicateGroup, PluginGroupMerge,
        PluginHook, PluginHookStats, PluginHost, PluginKeepers, PluginRef, PluginRunReport, PluginScope, PluginScopes,
        PluginSortKeys, VirtualId, XtreamCluster,
    },
    utils::Internable,
};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tuliprox_core::{
    model::{ConfigTarget, TransformStage},
    utils::StepMeasure,
};
use tuliprox_repository::MemoryPlaylistSource;

/// Renames one group in `after-filter`, the way the Phase S fixture does, and
/// counts how often it ran.
#[derive(Clone, Default)]
struct RenameGroupHost {
    calls: Arc<AtomicUsize>,
    abort: bool,
}

impl PluginHost for RenameGroupHost {
    fn wants(&self, hook: PluginHook, _target: &str) -> bool { hook == PluginHook::AfterFilter }

    fn on_stage(
        &self,
        hook: PluginHook,
        _ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> impl Future<Output = Result<(), PluginAbort>> + Send {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let abort = self.abort;
        async move {
            if abort {
                return Err(PluginAbort { plugin: "fixture".into(), hook, reason: "refused".to_string() });
            }
            let mut stats = PluginHookStats::new("fixture", hook);
            for item in items.iter_mut().filter(|item| item.header.group.as_ref() == "News") {
                item.header.group = "Renamed".intern();
                stats.modified += 1;
            }
            report.record(&stats);
            Ok(())
        }
    }
}

fn item(name: &str, group: &str) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            name: name.intern(),
            group: group.intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    }
}

fn items() -> Vec<PlaylistItem> { vec![item("One", "News"), item("Two", "Sports"), item("Drop", "Sports")] }

fn filtered_target() -> ConfigTarget {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter = get_filter(r#"name ~ "One|Two""#, None).expect("filter should parse").into();
    target
}

fn group_titles(groups: &[shared::model::PlaylistGroup]) -> Vec<String> {
    let mut titles: Vec<String> = groups.iter().map(|group| group.title.to_string()).collect();
    titles.sort();
    titles
}

#[tokio::test]
async fn after_filter_hook_sees_filtered_items_and_its_group_rename_is_regrouped() {
    let host = RenameGroupHost::default();
    let pipe = [TransformStage::Filter, TransformStage::Rename, TransformStage::Map];

    let (groups, outcome) = execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &host)
        .await
        .expect("hook must not abort");

    assert_eq!(host.calls.load(Ordering::Relaxed), 1, "after-filter runs once per input");
    assert_eq!(group_titles(&groups), ["Renamed", "Sports"]);
    assert_eq!(outcome.filter.as_ref().map(|filter| filter.removed), Some(1));
    let stats = &outcome.plugins.hooks;
    assert_eq!(stats.len(), 1, "one entry per (plugin, hook)");
    assert_eq!((stats[0].hook.as_str(), stats[0].modified), ("after-filter", 1));
    assert_eq!(outcome.to_stats().plugins.hooks, outcome.plugins.hooks, "the report rides PipelineStats");
}

#[tokio::test]
async fn after_filter_hook_fires_wherever_processing_order_puts_the_filter() {
    let host = RenameGroupHost::default();
    let pipe = [TransformStage::Map, TransformStage::Rename, TransformStage::Filter];

    let (groups, _) = execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &host)
        .await
        .expect("hook must not abort");

    assert_eq!(host.calls.load(Ordering::Relaxed), 1);
    assert_eq!(group_titles(&groups), ["Renamed", "Sports"]);
}

#[tokio::test]
async fn no_plugins_leaves_the_pipeline_output_unchanged() {
    let pipe = [TransformStage::Filter, TransformStage::Rename, TransformStage::Map];

    let (groups, _) = execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &NoPlugins)
        .await
        .expect("NoPlugins never aborts");

    assert_eq!(group_titles(&groups), ["News", "Sports"]);
}

#[tokio::test]
async fn abort_target_failure_propagates_as_an_error() {
    let host = RenameGroupHost { abort: true, ..RenameGroupHost::default() };

    let result =
        execute_pipeline_on_items(items(), &filtered_target(), "input", &[TransformStage::Filter], &host).await;

    let abort = result.expect_err("abort-target must stop the pipeline");
    assert_eq!(abort.hook, PluginHook::AfterFilter);
}

// ——— W5: stage hooks, after-sort, strategies, on-playlist-loaded ———

/// What the [`Spy`] answers. Every field is a canned decision.
#[derive(Default)]
struct Plan {
    reverse_after_sort: bool,
    after_sort: Option<Vec<usize>>,
    sort_keys: Option<PluginSortKeys>,
    chno: Option<PluginChnoMap>,
    keepers: Option<PluginKeepers>,
    merge: Option<PluginGroupMerge>,
    loaded_drop: Option<&'static str>,
    loaded_group: Option<&'static str>,
}

/// A host that wants every hook, logs each call and answers from its [`Plan`].
#[derive(Clone, Default)]
struct Spy {
    log: Arc<Mutex<Vec<String>>>,
    plan: Arc<Plan>,
}

impl Spy {
    fn new(plan: Plan) -> Self { Self { log: Arc::default(), plan: Arc::new(plan) } }

    fn record(&self, entry: impl Into<String>) { self.log.lock().unwrap().push(entry.into()); }

    fn calls(&self) -> Vec<String> { self.log.lock().unwrap().clone() }

    fn count(&self, prefix: &str) -> usize { self.calls().iter().filter(|call| call.starts_with(prefix)).count() }
}

impl PluginHost for Spy {
    fn wants(&self, _hook: PluginHook, _target: &str) -> bool { true }

    fn scope(&self, hook: PluginHook, _ctx: &HookCtx<'_>) -> Option<PluginScopes> {
        (hook == PluginHook::PlaylistLoaded)
            .then(|| PluginScopes(vec![Arc::new(PluginScope::new(&["^Scoped".to_string()], &[]).unwrap())]))
    }

    async fn on_stage(
        &self,
        hook: PluginHook,
        _ctx: &HookCtx<'_>,
        _items: &mut Vec<PlaylistItem>,
        _report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        self.record(hook.as_str());
        Ok(())
    }

    async fn on_playlist_loaded(
        &self,
        ctx: &HookCtx<'_>,
        items: &mut Vec<PlaylistItem>,
        report: &mut PluginRunReport,
    ) -> Result<(), PluginAbort> {
        self.record(format!("playlist-loaded:{}:{}", ctx.input.unwrap_or_default(), items.len()));
        let mut stats = PluginHookStats::new("spy", PluginHook::PlaylistLoaded);
        if let Some(marker) = self.plan.loaded_drop {
            let before = items.len();
            items.retain(|item| !item.header.name.contains(marker));
            stats.dropped = u64::try_from(before - items.len()).unwrap();
        }
        if let Some(group) = self.plan.loaded_group {
            for item in items.iter_mut() {
                item.header.group = group.intern();
                stats.modified += 1;
            }
        }
        report.record(&stats);
        Ok(())
    }

    async fn after_sort(
        &self,
        ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> Result<Option<Vec<usize>>, PluginAbort> {
        self.record(format!("after-sort:{}", ctx.view.unwrap_or_default()));
        Ok(self
            .plan
            .after_sort
            .clone()
            .or_else(|| self.plan.reverse_after_sort.then(|| (0..items.len()).rev().collect())))
    }

    async fn sort_keys(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> Result<Option<PluginSortKeys>, PluginAbort> {
        self.record("sort-key");
        Ok(self.plan.sort_keys.clone())
    }

    async fn assign_chno(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        _items: &[&PlaylistItem],
        _report: &mut PluginRunReport,
    ) -> Result<Option<PluginChnoMap>, PluginAbort> {
        self.record("assign-chno");
        Ok(self.plan.chno.clone())
    }

    async fn resolve_duplicates(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        groups: &[PluginDuplicateGroup<'_>],
        _report: &mut PluginRunReport,
    ) -> Result<Option<PluginKeepers>, PluginAbort> {
        self.record(format!("resolve-duplicates:{}", groups.len()));
        Ok(self.plan.keepers.clone())
    }

    async fn merge_groups(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        groups: &[String],
        _report: &mut PluginRunReport,
    ) -> Result<Option<PluginGroupMerge>, PluginAbort> {
        self.record(format!("merge-groups:{}", groups.join(",")));
        Ok(self.plan.merge.clone())
    }
}

fn plugin_ref() -> PluginRef { "plugin:probe".parse().expect("valid plugin reference") }

fn named_item(name: &str, group: &str) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            name: name.intern(),
            title: name.intern(),
            group: group.intern(),
            input_stream_id: name.intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    }
}

fn view_group(id: u32, title: &str, names: &[&str]) -> PlaylistGroup {
    PlaylistGroup {
        id,
        title: title.intern(),
        channels: names.iter().map(|name| named_item(name, title)).collect(),
        xtream_cluster: XtreamCluster::Live,
    }
}

/// `A: a1 a2`, `B: b1`: three channels in two groups.
fn two_groups() -> Vec<PlaylistGroup> { vec![view_group(1, "A", &["a1", "a2"]), view_group(2, "B", &["b1"])] }

fn layout(groups: &[PlaylistGroup]) -> Vec<String> {
    groups
        .iter()
        .map(|group| {
            let names: Vec<_> = group.channels.iter().map(|channel| channel.header.name.to_string()).collect();
            format!("{}:{}", group.title, names.join(","))
        })
        .collect()
}

fn chnos(groups: &[PlaylistGroup]) -> Vec<u32> {
    groups.iter().flat_map(|group| group.channels.iter().map(|channel| channel.header.chno)).collect()
}

async fn finalize<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    playlist: Vec<PlaylistGroup>,
    view: &str,
    shared_chnos: &mut Option<std::collections::HashMap<(shared::model::UUIDType, u32), u32>>,
) -> Vec<PlaylistGroup> {
    let mut step = StepMeasure::new("test", |_, _| {});
    finalize_playlist_view(plugins, target, playlist, &mut step, view, shared_chnos, &mut PluginRunReport::default())
        .await
        .expect("no plugin aborts")
}

#[tokio::test]
async fn stage_hooks_fire_around_every_stage_in_pipe_order() {
    let host = Spy::default();
    let pipe = [TransformStage::Filter, TransformStage::Rename, TransformStage::Map];

    execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &host).await.expect("no abort");

    assert_eq!(
        host.calls(),
        ["before-filter", "after-filter", "before-rename", "after-rename", "before-map", "after-map"]
    );
}

#[tokio::test]
async fn stage_hooks_follow_a_custom_processing_order() {
    let host = Spy::default();
    let pipe = [TransformStage::Map, TransformStage::Filter];

    execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &host).await.expect("no abort");

    assert_eq!(host.calls(), ["before-map", "after-map", "before-filter", "after-filter"]);
}

#[tokio::test]
async fn stage_hooks_do_not_fire_for_the_series_episode_pipeline() {
    let host = Spy::default();
    let pipe = [TransformStage::Filter, TransformStage::Rename, TransformStage::Map];
    execute_pipeline_on_items(items(), &filtered_target(), "input", &pipe, &host).await.expect("no abort");
    assert_eq!(host.calls().len(), 6);

    // `execute_pipeline_on_groups` takes no host at all: running it must not add a call.
    let (groups, _) = execute_pipeline_on_groups(two_groups(), &filtered_target(), &pipe);

    assert_eq!(host.calls().len(), 6, "episodes never re-fire the stage hooks");
    assert!(groups.iter().all(|group| group.channels.is_empty() || !group.title.is_empty()));
}

#[tokio::test]
async fn after_sort_runs_once_per_view_and_applies_the_permutation() {
    let host = Spy::new(Plan { reverse_after_sort: true, ..Plan::default() });
    let target = ConfigTarget::from(&ConfigTargetDto::default());
    let baseline = finalize(&NoPlugins, &target, two_groups(), "base", &mut None).await;
    assert_eq!(layout(&baseline), ["A:a1,a2", "B:b1"]);

    let base = finalize(&host, &target, two_groups(), "base", &mut None).await;
    assert_eq!(host.count("after-sort"), 1, "one call for the base view");
    // Reversed flat order [b1, a2, a1]: channels reorder inside their group, groups by first channel.
    assert_eq!(layout(&base), ["B:b1", "A:a2,a1"]);

    finalize(&host, &target, two_groups(), "Xtream", &mut None).await;
    assert_eq!(host.calls().iter().filter(|call| call.starts_with("after-sort")).count(), 2);
    assert_eq!(host.calls(), ["after-sort:base", "after-sort:Xtream"]);
}

#[tokio::test]
async fn an_after_sort_answer_that_is_not_a_permutation_is_ignored() {
    let target = ConfigTarget::from(&ConfigTargetDto::default());
    for bad in [vec![0, 0, 1], vec![2, 1], vec![0, 1, 7]] {
        let host = Spy::new(Plan { after_sort: Some(bad), ..Plan::default() });
        let result = finalize(&host, &target, two_groups(), "base", &mut None).await;
        assert_eq!(layout(&result), ["A:a1,a2", "B:b1"]);
    }
}

#[tokio::test]
async fn sort_key_strategy_stable_sorts_channels_within_their_group() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.plugin_strategies.sort_key = Some(plugin_ref());
    // Flat view order is [a1, a2, b1]; the plugin wants a2 before a1.
    let host = Spy::new(Plan { sort_keys: Some(vec!["2".into(), "1".into(), "1".into()]), ..Plan::default() });

    let result = finalize(&host, &target, two_groups(), "base", &mut None).await;

    assert_eq!(host.count("sort-key"), 1);
    assert_eq!(layout(&result), ["A:a2,a1", "B:b1"]);
}

#[tokio::test]
async fn sort_key_answer_of_the_wrong_length_is_ignored() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.plugin_strategies.sort_key = Some(plugin_ref());
    let host = Spy::new(Plan { sort_keys: Some(vec!["2".into()]), ..Plan::default() });

    let result = finalize(&host, &target, two_groups(), "base", &mut None).await;

    assert_eq!(layout(&result), ["A:a1,a2", "B:b1"]);
}

#[tokio::test]
async fn chno_strategy_numbers_listed_channels_and_the_host_numbers_the_rest() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.plugin_strategies.assign_chno = Some(plugin_ref());
    // a1 -> 500, b1 -> 7; a2 is unlisted.
    let host = Spy::new(Plan { chno: Some(vec![(0, 500), (2, 7)]), ..Plan::default() });

    let result = finalize(&host, &target, two_groups(), "base", &mut None).await;

    // The host numbers the unlisted channel from 1, skipping numbers already taken.
    assert_eq!(chnos(&result), [500, 1, 7]);
}

#[tokio::test]
async fn chno_numbers_are_computed_once_and_shared_by_both_views() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.plugin_strategies.assign_chno = Some(plugin_ref());
    let host = Spy::new(Plan { chno: Some(vec![(0, 500), (1, 501), (2, 502)]), ..Plan::default() });
    let mut shared_chnos = None;

    let base = finalize(&host, &target, two_groups(), "base", &mut shared_chnos).await;
    let xtream = finalize(&host, &target, two_groups(), "Xtream", &mut shared_chnos).await;

    assert_eq!(host.count("assign-chno"), 1, "the plugin is asked once per target");
    assert_eq!(chnos(&base), [500, 501, 502]);
    assert_eq!(chnos(&xtream), chnos(&base), "a channel has one number in both views");
}

fn dedupe_target(keep: DeduplicateKeep) -> ConfigTarget {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.post_merge_content_dedup =
        Some(DeduplicateConfig { match_by: DeduplicateMatchBy::Name, keep, match_as_ascii: false });
    target
}

async fn dedupe_with(host: &Spy, target: &ConfigTarget) -> Vec<String> {
    // "News" and "News HD" share the normalized key `news`; "Sports" is alone.
    let playlist = vec![view_group(1, "G", &["News", "News HD", "Sports"])];
    let mut step = StepMeasure::new("test", |_, _| {});
    let catalog = prepare_eligible_catalog(host, target, playlist, &mut step, &mut PluginRunReport::default())
        .await
        .expect("no plugin aborts");
    layout(&catalog)
}

#[tokio::test]
async fn resolve_duplicates_strategy_decides_which_member_survives() {
    let mut target = dedupe_target(DeduplicateKeep::Plugin(plugin_ref()));
    target.execution_plan.plugin_strategies.resolve_duplicates = Some(plugin_ref());

    // The host would keep the best quality ("News HD"); the plugin keeps the first member.
    let host = Spy::new(Plan { keepers: Some(vec![vec![0]]), ..Plan::default() });
    assert_eq!(dedupe_with(&host, &target).await, ["G:News,Sports"]);
    assert_eq!(host.count("resolve-duplicates:1"), 1, "only the group of more than one member is sent");

    // An empty keeper list falls back to the host's keep.
    let host = Spy::new(Plan { keepers: Some(vec![vec![]]), ..Plan::default() });
    assert_eq!(dedupe_with(&host, &target).await, ["G:News HD,Sports"]);
}

#[tokio::test]
async fn merge_groups_strategy_renames_members_so_the_catalog_merges_them() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.plugin_strategies.merge_groups = Some(plugin_ref());
    let host = Spy::new(Plan {
        merge: Some(vec![("Sports".to_string(), vec!["Sport".to_string(), "SPORTS HD".to_string()])]),
        ..Plan::default()
    });
    let playlist =
        vec![view_group(1, "Sport", &["s1"]), view_group(2, "News", &["n1"]), view_group(3, "SPORTS HD", &["s2"])];
    let mut step = StepMeasure::new("test", |_, _| {});

    let catalog = prepare_eligible_catalog(&host, &target, playlist, &mut step, &mut PluginRunReport::default())
        .await
        .expect("no plugin aborts");

    assert_eq!(host.calls(), ["merge-groups:Sport,News,SPORTS HD"]);
    assert_eq!(layout(&catalog), ["Sports:s1,s2", "News:n1"]);
    assert!(catalog[0].channels.iter().all(|channel| &*channel.header.group == "Sports"));
}

#[tokio::test]
async fn no_plugins_leaves_the_finalize_side_untouched_even_when_strategies_are_configured() {
    let mut target = dedupe_target(DeduplicateKeep::Plugin(plugin_ref()));
    let strategies = &mut target.execution_plan.plugin_strategies;
    strategies.sort_key = Some(plugin_ref());
    strategies.assign_chno = Some(plugin_ref());
    strategies.resolve_duplicates = Some(plugin_ref());
    strategies.merge_groups = Some(plugin_ref());

    let view = finalize(&NoPlugins, &target, two_groups(), "base", &mut None).await;
    assert_eq!(layout(&view), ["A:a1,a2", "B:b1"]);
    assert_eq!(chnos(&view), [1, 2, 3], "host numbering");

    let mut step = StepMeasure::new("test", |_, _| {});
    let playlist = vec![view_group(1, "G", &["News", "News HD", "Sports"])];
    let catalog = prepare_eligible_catalog(&NoPlugins, &target, playlist, &mut step, &mut PluginRunReport::default())
        .await
        .expect("NoPlugins never aborts");
    assert_eq!(layout(&catalog), ["G:News HD,Sports"], "the host keeps the best quality");
}

fn loaded_items() -> Vec<PlaylistGroup> {
    vec![view_group(1, "Scoped one", &["keep1", "drop1", "keep2"]), view_group(2, "Other", &["other1", "drop2"])]
}

#[tokio::test]
async fn playlist_loaded_hook_materializes_only_the_scope_and_writes_back_drops_and_edits() {
    let host = Spy::new(Plan { loaded_drop: Some("drop"), loaded_group: Some("Scoped moved"), ..Plan::default() });
    let mut report = PluginRunReport::default();
    let source = MemoryPlaylistSource::new(loaded_items()).into_source();

    let mut hooked = apply_playlist_loaded_hook(&host, "provider", source, &mut report)
        .await
        .unwrap_or_else(|(_, abort)| panic!("no abort expected: {abort}"));

    // Only the three `Scoped*` items were sent; `Other` stayed out of the hook.
    assert_eq!(host.calls(), ["playlist-loaded:provider:3"]);
    // `drop1` is gone, the survivors are edited (regrouped); `Other` keeps its own drop2.
    assert_eq!(layout(&hooked.take_groups()), ["Scoped moved:keep1,keep2", "Other:other1,drop2"]);
    assert_eq!(report.hooks.len(), 1);
    assert_eq!((report.hooks[0].dropped, report.hooks[0].modified), (1, 2));
}

#[tokio::test]
async fn playlist_loaded_hook_skips_a_source_nothing_in_scope_matches() {
    let host = Spy::new(Plan { loaded_drop: Some("drop"), ..Plan::default() });
    let source = MemoryPlaylistSource::new(vec![view_group(1, "Other", &["x", "drop"])]).into_source();

    let mut hooked = apply_playlist_loaded_hook(&host, "provider", source, &mut PluginRunReport::default())
        .await
        .unwrap_or_else(|(_, abort)| panic!("no abort expected: {abort}"));

    assert!(host.calls().is_empty(), "no instance is spent on an input the scope never matches");
    assert_eq!(layout(&hooked.take_groups()), ["Other:x,drop"]);
}

#[tokio::test]
async fn playlist_loaded_hook_is_a_no_op_without_plugins() {
    let source = MemoryPlaylistSource::new(loaded_items()).into_source();

    let mut hooked = apply_playlist_loaded_hook(&NoPlugins, "provider", source, &mut PluginRunReport::default())
        .await
        .unwrap_or_else(|(_, abort)| panic!("no abort expected: {abort}"));

    assert_eq!(layout(&hooked.take_groups()), ["Scoped one:keep1,drop1,keep2", "Other:other1,drop2"]);
}

#[tokio::test]
async fn playlist_loaded_hook_realigns_duplicate_or_empty_ids_without_shifting_slots() {
    let host = Spy::new(Plan { loaded_drop: Some("drop"), ..Plan::default() });
    let mut item1 = named_item("drop1", "Scoped A");
    item1.header.input_stream_id = "".intern();
    item1.header.virtual_id = VirtualId::default();

    let mut item2 = named_item("other2", "Other A");
    item2.header.input_stream_id = "".intern();
    item2.header.virtual_id = VirtualId::default();

    let mut item3 = named_item("keep3", "Scoped B");
    item3.header.input_stream_id = "".intern();
    item3.header.virtual_id = VirtualId::default();

    let source = MemoryPlaylistSource::new(vec![
        PlaylistGroup {
            id: 1,
            title: "Scoped A".intern(),
            channels: vec![item1, item2],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup { id: 2, title: "Scoped B".intern(), channels: vec![item3], xtream_cluster: XtreamCluster::Live },
    ])
    .into_source();

    let mut hooked = apply_playlist_loaded_hook(&host, "provider", source, &mut PluginRunReport::default())
        .await
        .unwrap_or_else(|(_, abort)| panic!("no abort expected: {abort}"));

    assert_eq!(layout(&hooked.take_groups()), ["Other A:other2", "Scoped B:keep3"]);
}

#[derive(Clone, Default)]
struct FilterSpy;

impl PluginHost for FilterSpy {
    fn eval_filter_batch(
        &self,
        _plugin: &str,
        _func: &str,
        _args: &[String],
        _ctx: &HookCtx<'_>,
        items: &[&PlaylistItem],
        report: &mut PluginRunReport,
    ) -> impl Future<Output = Vec<bool>> + Send {
        let mut stats = PluginHookStats::new("test-filter-plugin", PluginHook::FilterEval);
        stats.kept = items.len() as u64;
        report.record(&stats);
        let decisions = vec![true; items.len()];
        async move { decisions }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn series_episode_pipeline_merges_filter_eval_report_into_caller_report() {
    use crate::{
        fetched_playlist::FetchedPlaylist,
        processor::{
            playlist::{ingest::PlaylistProcessingContext, PlaylistUpdateRunOrder, ProcessingPipe},
            stalker_refresh::StalkerRefreshMode,
            xtream_series::playlist_resolve_series,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        foundation::get_target_processing_filter,
        model::{
            ConfigPaths, PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
            SeriesStreamProperties, StreamProperties,
        },
    };
    use tuliprox_core::{
        model::{AppConfig, Config, ConfigInput, ProcessTargets, SourcesConfig},
        utils::FileLockManager,
    };

    let host = FilterSpy;
    let filter = get_target_processing_filter("plugin.test-filter-plugin.keep()", None).unwrap();
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter.processing = Some(filter);

    let episodes = vec![SeriesStreamDetailEpisodeProperties { id: 101, ..Default::default() }];
    let series_props = SeriesStreamProperties {
        series_id: 100,
        details: Some(SeriesStreamDetailProperties::new(None, Vec::new(), Some(episodes))),
        ..Default::default()
    };
    let item = PlaylistItem {
        header: PlaylistItemHeader {
            id: "100".intern(),
            name: "Show 1".intern(),
            title: "Show 1".intern(),
            group: "Shows".intern(),
            item_type: PlaylistItemType::SeriesInfo,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
            ..Default::default()
        },
    };
    let group =
        PlaylistGroup { id: 1, title: "Shows".intern(), channels: vec![item], xtream_cluster: XtreamCluster::Series };

    let dir = tempfile::tempdir().unwrap();
    let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
    let app_config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        plugins: Arc::default(),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
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
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
    };

    let ctx: PlaylistProcessingContext<shared::model::NoopSink, crate::metadata_sink::NoopMetadataSink, _> =
        PlaylistProcessingContext {
            client: reqwest::Client::new(),
            run_id: "test-series-report".into(),
            execution_order: PlaylistUpdateRunOrder::from(1),
            config: Arc::new(app_config),
            user_targets: Arc::new(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            }),
            events: shared::model::NoopSink,
            playlist_state: None,
            disabled_headers: None,
            processed_inputs: Arc::default(),
            input_completions: Arc::default(),
            input_locks: Arc::default(),
            provider_manager: None,
            metadata_manager: None,
            pre_processed_inputs: None,
            stalker_refresh_mode: StalkerRefreshMode::Complete,
            partial_refresh: Arc::default(),
            had_quality_rejections: Arc::default(),
            input_refresh: None,
            library_update_mode: crate::processor::playlist::LibraryUpdateMode::default(),
            maintenance: None,
            plugins: host,
        };

    let input = ConfigInput { name: "test-input".intern(), ..ConfigInput::default() };
    let mut provider_fpl = FetchedPlaylist {
        input: &input,
        source: MemoryPlaylistSource::new(vec![group.clone()]).into_source(),
        epg: None,
    };
    let mut processed_fpl =
        FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(vec![group]).into_source(), epg: None };

    let mut errors = Vec::new();
    let pipe: ProcessingPipe = vec![TransformStage::Filter];
    let mut report = PluginRunReport::default();

    playlist_resolve_series(&ctx, &target, &mut errors, &pipe, &mut provider_fpl, &mut processed_fpl, &mut report)
        .await;

    assert!(!report.hooks.is_empty(), "series episode plugin report must be merged into target's report");
    assert_eq!(report.hooks[0].plugin, "test-filter-plugin");
    assert_eq!(report.hooks[0].hook, PluginHook::FilterEval.as_str());
    assert_eq!(report.hooks[0].kept, 1);
}
