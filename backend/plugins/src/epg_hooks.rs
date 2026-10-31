//! EPG and metadata hooks: `on-epg-loaded`, `transform-programmes`,
//! `enrich-programmes`, `match-channel` and `enrich-metadata`.
//!
//! All five are fail-open: a plugin that traps, times out or returns `err`
//! leaves the data exactly as the previous plugin left it, and the failure is
//! logged, counted and fed to the circuit breaker (`HookDispatcher::settle`,
//! whose abort is ignored here). Each plugin runs in one fresh instance per
//! call, in priority order, over what the plugin before it produced. A plugin's
//! results are applied only when its whole run succeeded.

use crate::{
    convert::item_to_channel,
    convert_epg::{channel_to_wit, merge_loaded, programme_from_wit, programme_to_wit, ChannelIds},
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
};
use shared::model::{
    EpgProgramme, HookCtx, PlaylistItem, PluginChannelProgrammes, PluginEpgChannel, PluginHook, PluginHookStats,
    PluginMatchRequest, PluginRunReport,
};
use std::time::Instant;
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::epg_match_hooks::MatchRequest, tuliprox::plugin::epg_types::EpgProgramme as WitProgramme,
};

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` function")]
fn bind(err: wasmtime::Error) -> CallError { CallError::Trap(format!("{err:#}")) }

fn missing(interface: &str) -> CallError { CallError::Instantiate(format!("does not export {interface}")) }

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` function")]
fn hook_err(err: String) -> CallError { CallError::Trap(format!("hook returned err: {err}")) }

/// Close one plugin's run. EPG and metadata hooks are fail-open only, so the
/// abort `settle` computes for `fail_mode: abort-target` is dropped.
#[allow(clippy::too_many_arguments, reason = "one call per plugin run, mirrors HookDispatcher::settle")]
fn close(
    plugin: &PluginRecord,
    loaded: &LoadedPlugin,
    hook: PluginHook,
    ctx: &HookCtx<'_>,
    stats: PluginHookStats,
    started: Instant,
    outcome: &Result<(), CallError>,
    report: &mut PluginRunReport,
) {
    let _ = HookDispatcher::settle(plugin, loaded, hook, ctx, stats, started, outcome.as_ref().copied(), report);
}

pub(crate) async fn on_epg_loaded(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    channels: &mut [PluginEpgChannel],
    report: &mut PluginRunReport,
) {
    if channels.is_empty() {
        return;
    }
    let hook = PluginHook::EpgLoaded;
    let batch_size = dispatcher.batch_size();
    for (plugin, loaded) in dispatcher.runnable(hook) {
        let started = Instant::now();
        let mut stats = PluginHookStats::new(&plugin.id, hook);
        stats.kept = count(channels.len());
        let outcome = match load_one(dispatcher, loaded, ctx, channels, batch_size).await {
            Ok(returned) => {
                let merge = merge_loaded(channels, returned);
                stats.modified = count(merge.modified);
                stats.ignored_changes = count(merge.ignored);
                Ok(())
            }
            Err(err) => Err(err),
        };
        close(plugin, loaded, hook, ctx, stats, started, &outcome, report);
    }
}

/// One plugin over every channel, `batch_size` per guest call.
async fn load_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    channels: &[PluginEpgChannel],
    batch_size: usize,
) -> Result<Vec<tuliprox_plugin_api::bindings::tuliprox::plugin::epg_types::EpgChannel>, CallError> {
    let indices = loaded.epg_load_hooks.as_ref().ok_or_else(|| missing("epg-load-hooks"))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let source = ctx.input.unwrap_or_default();
    let mut returned = Vec::new();
    for chunk in channels.chunks(batch_size) {
        let wit: Vec<_> = chunk.iter().map(channel_to_wit).collect();
        let limits = live.limits();
        let result = call_with_deadline(limits, guest.call_on_epg_loaded(live.armed(), source, &wit)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        returned.extend(result?.map_err(hook_err)?);
    }
    Ok(returned)
}

pub(crate) async fn transform_programmes(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    programmes: &mut Vec<EpgProgramme>,
    report: &mut PluginRunReport,
) {
    if programmes.is_empty() {
        return;
    }
    let hook = PluginHook::TransformProgrammes;
    let batch_size = dispatcher.batch_size();
    for (plugin, loaded) in dispatcher.runnable(hook) {
        let started = Instant::now();
        let mut stats = PluginHookStats::new(&plugin.id, hook);
        let outcome = match transform_one(dispatcher, loaded, ctx, programmes, batch_size).await {
            Ok((kept, ignored)) => {
                stats.kept = count(kept.len());
                stats.dropped = count(programmes.len().saturating_sub(kept.len()));
                stats.ignored_changes = count(ignored);
                *programmes = kept;
                Ok(())
            }
            Err(err) => {
                stats.kept = count(programmes.len());
                Err(err)
            }
        };
        close(plugin, loaded, hook, ctx, stats, started, &outcome, report);
    }
}

/// One plugin over the whole batch; returns the programmes to keep and how many
/// returned ones named a channel the batch did not carry (dropped).
async fn transform_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    programmes: &[EpgProgramme],
    batch_size: usize,
) -> Result<(Vec<EpgProgramme>, usize), CallError> {
    let indices = loaded.epg_transform_hooks.as_ref().ok_or_else(|| missing("epg-transform-hooks"))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let source = ctx.input.unwrap_or_default();
    let ids = ChannelIds::of(programmes);
    let mut kept = Vec::with_capacity(programmes.len());
    let mut ignored = 0usize;
    for chunk in programmes.chunks(batch_size) {
        let wit: Vec<WitProgramme> = chunk.iter().map(programme_to_wit).collect();
        let limits = live.limits();
        let result = call_with_deadline(limits, guest.call_transform_programmes(live.armed(), source, &wit)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        for programme in result?.map_err(hook_err)? {
            match programme_from_wit(programme, &ids) {
                Some(programme) => kept.push(programme),
                None => ignored += 1,
            }
        }
    }
    Ok((kept, ignored))
}

pub(crate) async fn enrich_programmes(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    channels: &mut [PluginChannelProgrammes],
    report: &mut PluginRunReport,
) {
    if channels.is_empty() {
        return;
    }
    let hook = PluginHook::EnrichProgrammes;
    for (plugin, loaded) in dispatcher.runnable(hook) {
        let started = Instant::now();
        let mut stats = PluginHookStats::new(&plugin.id, hook);
        let outcome = match enrich_one(dispatcher, loaded, channels).await {
            Ok((results, ignored)) => {
                let before: usize = channels.iter().map(|channel| channel.programmes.len()).sum();
                let after: usize = results.iter().map(Vec::len).sum();
                stats.kept = count(after);
                stats.dropped = count(before.saturating_sub(after));
                stats.modified = count(after.saturating_sub(before));
                stats.ignored_changes = count(ignored);
                for (channel, mut programmes) in channels.iter_mut().zip(results) {
                    programmes.sort_by_key(|programme| programme.start);
                    channel.programmes = programmes;
                }
                Ok(())
            }
            Err(err) => {
                stats.kept = count(channels.iter().map(|channel| channel.programmes.len()).sum());
                Err(err)
            }
        };
        close(plugin, loaded, hook, ctx, stats, started, &outcome, report);
    }
}

/// One plugin over every channel: one instance, one guest call per channel.
/// Nothing is applied here; the results are index-aligned with `channels`.
async fn enrich_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    channels: &[PluginChannelProgrammes],
) -> Result<(Vec<Vec<EpgProgramme>>, usize), CallError> {
    let indices = loaded.epg_enrich_hooks.as_ref().ok_or_else(|| missing("epg-enrich-hooks"))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let mut results = Vec::with_capacity(channels.len());
    let mut ignored = 0usize;
    for channel in channels {
        let ids = ChannelIds::single(&channel.channel_id);
        let wit: Vec<WitProgramme> = channel
            .programmes
            .iter()
            .map(|programme| {
                let mut wit = programme_to_wit(programme);
                // The channel is the call's argument; a programme injected earlier carries it too.
                wit.channel = channel.channel_id.to_string();
                wit
            })
            .collect();
        let limits = live.limits();
        let result =
            call_with_deadline(limits, guest.call_enrich_programmes(live.armed(), &channel.channel_id, &wit)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let returned = result?.map_err(hook_err)?;
        let returned_len = returned.len();
        let programmes: Vec<EpgProgramme> =
            returned.into_iter().filter_map(|programme| programme_from_wit(programme, &ids)).collect();
        ignored += returned_len - programmes.len();
        results.push(programmes);
    }
    Ok((results, ignored))
}

pub(crate) async fn match_channels(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    requests: &[PluginMatchRequest<'_>],
    report: &mut PluginRunReport,
) -> Vec<Option<String>> {
    let mut answers: Vec<Option<String>> = vec![None; requests.len()];
    if requests.is_empty() {
        return answers;
    }
    let hook = PluginHook::MatchChannel;
    let batch_size = dispatcher.batch_size();
    for (plugin, loaded) in dispatcher.runnable(hook) {
        // Miss-only chaining: a later plugin is asked only about what the earlier ones left open.
        let open: Vec<usize> = (0..requests.len()).filter(|&index| answers[index].is_none()).collect();
        if open.is_empty() {
            break;
        }
        let started = Instant::now();
        let mut stats = PluginHookStats::new(&plugin.id, hook);
        let outcome = match match_one(dispatcher, loaded, requests, &open, batch_size).await {
            Ok(found) => {
                let matched = found.iter().filter(|answer| answer.is_some()).count();
                stats.modified = count(matched);
                stats.kept = count(open.len() - matched);
                for (&index, answer) in open.iter().zip(found) {
                    answers[index] = answer;
                }
                Ok(())
            }
            Err(err) => {
                stats.kept = count(open.len());
                Err(err)
            }
        };
        close(plugin, loaded, hook, ctx, stats, started, &outcome, report);
    }
    answers
}

/// One plugin over the requests at `open`. The answers are index-aligned with
/// `open`, and each is the shortlist's own spelling of the id the guest named,
/// or `None` when it named no candidate of that request.
async fn match_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    requests: &[PluginMatchRequest<'_>],
    open: &[usize],
    batch_size: usize,
) -> Result<Vec<Option<String>>, CallError> {
    let indices = loaded.epg_match_hooks.as_ref().ok_or_else(|| missing("epg-match-hooks"))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let mut found = Vec::with_capacity(open.len());
    for chunk in open.chunks(batch_size) {
        let wit: Vec<MatchRequest> = chunk
            .iter()
            .enumerate()
            .map(|(handle, &index)| MatchRequest {
                chan: item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), requests[index].item),
                candidates: requests[index].candidates.iter().map(channel_to_wit).collect(),
            })
            .collect();
        let limits = live.limits();
        let result = call_with_deadline(limits, guest.call_match_channels(live.armed(), &wit)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let answers = result?.map_err(hook_err)?;
        if answers.len() != chunk.len() {
            return Err(CallError::Trap(format!(
                "match-channels returned {} answers for {} requests",
                answers.len(),
                chunk.len()
            )));
        }
        for (&index, answer) in chunk.iter().zip(answers) {
            found.push(answer.and_then(|id| shortlisted(&requests[index].candidates, &id)));
        }
    }
    Ok(found)
}

/// The candidate id spelled as the shortlist spells it, if `answer` names one.
fn shortlisted(candidates: &[PluginEpgChannel], answer: &str) -> Option<String> {
    candidates.iter().find(|candidate| candidate.id.eq_ignore_ascii_case(answer)).map(|candidate| candidate.id.clone())
}

pub(crate) async fn enrich_metadata(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    item: &PlaylistItem,
    metadata_json: &str,
    report: &mut PluginRunReport,
) -> Option<String> {
    let hook = PluginHook::EnrichMetadata;
    let mut current: Option<String> = None;
    for (plugin, loaded) in dispatcher.runnable(hook) {
        let started = Instant::now();
        let mut stats = PluginHookStats::new(&plugin.id, hook);
        let input = current.as_deref().unwrap_or(metadata_json);
        let outcome = match metadata_one(dispatcher, loaded, item, input).await {
            Ok(Some(amended)) if amended != input => {
                stats.modified = 1;
                current = Some(amended);
                Ok(())
            }
            Ok(_) => {
                stats.kept = 1;
                Ok(())
            }
            Err(err) => {
                stats.kept = 1;
                Err(err)
            }
        };
        close(plugin, loaded, hook, ctx, stats, started, &outcome, report);
    }
    current
}

async fn metadata_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    item: &PlaylistItem,
    metadata_json: &str,
) -> Result<Option<String>, CallError> {
    let indices = loaded.metadata_hooks.as_ref().ok_or_else(|| missing("metadata-hooks"))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let channel = item_to_channel(0, item);
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_enrich_metadata(live.armed(), &channel, metadata_json)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result?.map_err(hook_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<PluginEpgChannel> {
        vec![
            PluginEpgChannel { id: "BBC.One".into(), names: vec![], icon: None },
            PluginEpgChannel { id: "itv.one".into(), names: vec![], icon: None },
        ]
    }

    #[test]
    fn an_answer_is_returned_in_the_shortlists_spelling_or_not_at_all() {
        let cases = [
            ("exact", "BBC.One", Some("BBC.One")),
            ("other case", "bbc.one", Some("BBC.One")),
            ("second", "ITV.ONE", Some("itv.one")),
            ("outside the shortlist", "bbc.two", None),
            ("empty", "", None),
        ];
        for (name, answer, expected) in cases {
            assert_eq!(shortlisted(&candidates(), answer).as_deref(), expected, "{name}");
        }
    }
}
