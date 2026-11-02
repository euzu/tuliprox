//! `eval-filter-batch`: index-aligned decision vectors for
//! `plugin.<id>.<fn>(…)` filter terms, prepared before the synchronous filter
//! evaluator runs.
//!
//! One call serves one term of one filter: the named plugin, the function name
//! and the literal arguments. The plugin sees every item of the view, in
//! order, in batches of `plugins.batch_size` with handles `0..len` per batch,
//! and answers one `bool` per channel. The host concatenates the batches into
//! one vector aligned to `items`.

use crate::{
    convert::item_to_channel,
    dispatch::{count, HookDispatcher},
    host_fns,
    runtime::{call_with_deadline, CallError},
};
use shared::model::{HookCtx, PlaylistItem, PluginHook, PluginHookStats, PluginRunReport};
use std::time::Instant;
use tuliprox_plugin_api::bindings::tuliprox::plugin::types::Channel;

/// Never aborts: a missing plugin, a failure or a wrong-length answer is all
/// `false`, counted in `report`.
pub(crate) async fn eval_filter_batch(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    func: &str,
    args: &[String],
    ctx: &HookCtx<'_>,
    items: &[&PlaylistItem],
    report: &mut PluginRunReport,
) -> Vec<bool> {
    let hook = PluginHook::FilterEval;
    let Some((record, loaded)) = dispatcher.named(hook, plugin) else {
        // Validation at plugin load rejects a reference to a missing plugin; this is a
        // plugin that went away (disabled or failed to reload) after the filter was checked.
        log::error!(
            "[plugin:{plugin}] {} on `{}`: no enabled plugin provides it, `plugin.{plugin}.{func}` is false",
            hook.as_str(),
            ctx.target
        );
        let mut stats = PluginHookStats::new(plugin, hook);
        stats.errors = 1;
        stats.dropped = count(items.len());
        report.record(&stats);
        return vec![false; items.len()];
    };
    if items.is_empty() {
        return Vec::new();
    }

    let started = Instant::now();
    let mut stats = PluginHookStats::new(&record.id, hook);
    let outcome = decide(dispatcher, loaded, func, args, items).await;
    let (decisions, outcome) = match outcome {
        Ok(decisions) => {
            let kept = decisions.iter().filter(|keep| **keep).count();
            stats.kept = count(kept);
            stats.dropped = count(decisions.len() - kept);
            (decisions, Ok(()))
        }
        Err(err) => {
            stats.dropped = count(items.len());
            (vec![false; items.len()], Err(err))
        }
    };
    // Filter calls never abort: a failed term is `false` and counted.
    let _ = HookDispatcher::settle(record, loaded, hook, ctx, stats, started, outcome.as_ref().copied(), report);
    decisions
}

/// All batches in one fresh instance. All or nothing: the first failing call, or an answer
/// whose length is not its batch's length, fails the run.
async fn decide(
    dispatcher: &HookDispatcher<'_>,
    loaded: &crate::registry::LoadedPlugin,
    func: &str,
    args: &[String],
    items: &[&PlaylistItem],
) -> Result<Vec<bool>, CallError> {
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let indices = loaded
        .filter_hooks
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export filter-hooks".to_string()))?;
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;

    let mut decisions = Vec::with_capacity(items.len());
    for chunk in items.chunks(dispatcher.batch_size()) {
        let channels: Vec<Channel> = chunk
            .iter()
            .enumerate()
            // A batch never exceeds MAX_PLUGINS_BATCH_SIZE, which fits in u32.
            .map(|(handle, item)| item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), item))
            .collect();
        let limits = live.limits();
        let answer =
            call_with_deadline(limits, guest.call_eval_filter_batch(live.armed(), func, &channels, args)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let answer = answer?.map_err(|err| CallError::Trap(format!("eval-filter-batch returned err: {err}")))?;
        if answer.len() != chunk.len() {
            return Err(CallError::Trap(format!(
                "eval-filter-batch returned {} decisions for {} channels",
                answer.len(),
                chunk.len()
            )));
        }
        decisions.extend(answer);
    }
    Ok(decisions)
}
