//! `type: plugin` inputs (plan §7 *Input providers*): `fetch-playlist` /
//! `fetch-epg` of the plugin an input names, under the provider deadline.
//!
//! Both calls are fail-open for the server: an `Err` only fails the one input
//! that asked, the same as an unreachable provider. The plugin gets one fresh
//! instance per call, with the provider deadline (`max_exec` × 10, never above
//! `max_task_exec`) instead of the hook deadline, since a call is a full
//! download.

use crate::{
    convert::{cluster_from_wit, item_type_from_wit},
    convert_epg::{programme_from_wit, ChannelIds},
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
};
use shared::{
    model::{
        EpgProgramme, HookCtx, PlaylistItem, PlaylistItemHeader, PluginEpgChannel, PluginHook, PluginHookStats,
        PluginInputRequest, PluginRunReport,
    },
    utils::{fnv1a_32_parts, Internable},
};
use std::{sync::Arc, time::Instant};
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::input_provider::InputRequest, tuliprox::plugin::types::Channel,
};

/// The `extra` key a plugin may use to hand the host a provider id for a
/// channel (the same key the host sends, see `convert::EXTRA_KEYS`).
const PROVIDER_ID_KEY: &str = "provider-id";

/// What an item without a group is filed under; an M3U would derive it from the
/// title, but a plugin that supplies no group has no title convention to read.
const DEFAULT_GROUP: &str = "Uncategorized";

/// Channels one `fetch-playlist` / `fetch-epg` call may return; beyond that the
/// call fails like a trap.
const MAX_RETURNED_CHANNELS: usize = 1_000_000;

fn check_channels(what: &str, returned: usize) -> Result<(), CallError> {
    if returned > MAX_RETURNED_CHANNELS {
        return Err(CallError::Trap(format!("{what} returned {returned} channels (cap {MAX_RETURNED_CHANNELS})")));
    }
    Ok(())
}

/// The enabled plugin `plugin` that runs `input.fetch`.
fn provider<'d>(
    dispatcher: &'d HookDispatcher<'_>,
    plugin: &str,
) -> Result<(&'d Arc<PluginRecord>, &'d LoadedPlugin), String> {
    dispatcher
        .named(PluginHook::FetchInput, plugin)
        .ok_or_else(|| format!("plugin {plugin} is not enabled or does not provide an input type"))
}

fn wit_request(request: &PluginInputRequest<'_>) -> InputRequest {
    InputRequest {
        input_name: request.input_name.to_string(),
        url: request.url.to_string(),
        username: request.username.map(ToString::to_string),
        password: request.password.map(ToString::to_string),
        options: request.options.to_vec(),
    }
}

/// Close one provider call: breaker, per-plugin stats and the log line.
///
/// `PluginHost` hands no run report to a provider call, so the fingerprint
/// `settle` records goes to a report nobody reads; `fail_mode: abort-target`
/// has nothing to abort here (the caller fails the input on any `Err`).
fn close<T>(
    plugin: &PluginRecord,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    kept: usize,
    started: Instant,
    outcome: &Result<T, CallError>,
) {
    let mut stats = PluginHookStats::new(&plugin.id, PluginHook::FetchInput);
    stats.kept = count(kept);
    let mut report = PluginRunReport::default();
    let _ = HookDispatcher::settle(
        plugin,
        loaded,
        PluginHook::FetchInput,
        ctx,
        stats,
        started,
        outcome.as_ref().map(|_| ()),
        &mut report,
    );
}

/// [`shared::model::PluginHost::fetch_playlist`].
pub(crate) async fn fetch_playlist(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginInputRequest<'_>,
) -> Result<Vec<PlaylistItem>, String> {
    let (record, loaded) = provider(dispatcher, plugin)?;
    let started = Instant::now();
    let outcome = call_fetch_playlist(dispatcher, loaded, request).await;
    let items = outcome.as_ref().map(Vec::len).unwrap_or_default();
    close(record, loaded, ctx, items, started, &outcome);
    outcome.map_err(|err| format!("plugin {plugin}: fetch-playlist failed: {err}"))
}

async fn call_fetch_playlist(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    request: &PluginInputRequest<'_>,
) -> Result<Vec<PlaylistItem>, CallError> {
    let indices = loaded
        .input_provider
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export input-provider".into()))?;
    let exec = loaded.prepared.limits().provider_exec(dispatcher.runtime().config());
    let mut live = dispatcher.open_with(loaded, exec).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();
    let channels = call_with_deadline(limits, guest.call_fetch_playlist(live.armed(), &wit_request(request))).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    let channels = channels?.map_err(|err| CallError::Trap(format!("fetch-playlist returned err: {err}")))?;
    check_channels("fetch-playlist", channels.len())?;
    Ok(channels_to_items(request.input_name, channels))
}

/// The host-issued `PlaylistItem` of each returned channel.
///
/// The plugin supplies the content (`name`, `title`, `group`, `url`, `epg-id`,
/// `logo`, `chno`, `item-type`, `cluster`). The host issues the rest, the way
/// the M3U parser does: `input_name` is the input, `source_ordinal` counts from
/// 1 in the order returned, and the provider id is `input-stream-id` when the
/// plugin set one, else the `provider-id` of `extra`, else a hash of input and
/// URL. A returned `handle`, `input-name` and `virtual-id` are ignored.
fn channels_to_items(input_name: &str, channels: Vec<Channel>) -> Vec<PlaylistItem> {
    let input_name: Arc<str> = input_name.intern();
    channels
        .into_iter()
        .enumerate()
        .map(|(index, channel)| {
            let provider_id = provider_id(&input_name, &channel);
            // 1-based like the M3U parser; a playlist never nears u32::MAX items.
            let source_ordinal = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let name = if channel.name.is_empty() { channel.title.clone() } else { channel.name.clone() };
            let title = if channel.title.is_empty() { name.clone() } else { channel.title };
            let group = if channel.group.trim().is_empty() { DEFAULT_GROUP.to_string() } else { channel.group };
            PlaylistItem {
                header: PlaylistItemHeader {
                    id: provider_id.intern(),
                    input_stream_id: channel.input_stream_id.trim().intern(),
                    name: name.intern(),
                    title: title.intern(),
                    group: group.intern(),
                    url: channel.url.intern(),
                    logo: channel.logo.intern(),
                    epg_channel_id: channel.epg_id.filter(|id| !id.trim().is_empty()).map(Internable::intern),
                    chno: channel.chno.unwrap_or(0),
                    item_type: item_type_from_wit(channel.item_type),
                    xtream_cluster: cluster_from_wit(channel.cluster),
                    input_name: Arc::clone(&input_name),
                    source_ordinal,
                    ..PlaylistItemHeader::default()
                },
            }
        })
        .collect()
}

fn provider_id(input_name: &str, channel: &Channel) -> String {
    let stream_id = channel.input_stream_id.trim();
    if !stream_id.is_empty() {
        return stream_id.to_string();
    }
    if let Some((_, id)) = channel.extra.iter().find(|(key, value)| key == PROVIDER_ID_KEY && !value.trim().is_empty())
    {
        return id.trim().to_string();
    }
    fnv1a_32_parts(&[input_name, &channel.url]).to_string()
}

/// [`shared::model::PluginHost::fetch_epg`].
pub(crate) async fn fetch_epg(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginInputRequest<'_>,
) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, String> {
    let (record, loaded) = provider(dispatcher, plugin)?;
    let started = Instant::now();
    let outcome = call_fetch_epg(dispatcher, loaded, request).await;
    let channels = outcome.as_ref().map(Vec::len).unwrap_or_default();
    close(record, loaded, ctx, channels, started, &outcome);
    outcome.map_err(|err| format!("plugin {plugin}: fetch-epg failed: {err}"))
}

async fn call_fetch_epg(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    request: &PluginInputRequest<'_>,
) -> Result<Vec<(PluginEpgChannel, Vec<EpgProgramme>)>, CallError> {
    let indices = loaded
        .input_provider
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export input-provider".into()))?;
    let exec = loaded.prepared.limits().provider_exec(dispatcher.runtime().config());
    let mut live = dispatcher.open_with(loaded, exec).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();
    let returned = call_with_deadline(limits, guest.call_fetch_epg(live.armed(), &wit_request(request))).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    let returned = returned?.map_err(|err| CallError::Trap(format!("fetch-epg returned err: {err}")))?;
    check_channels("fetch-epg", returned.len())?;
    Ok(returned
        .into_iter()
        .map(|(channel, programmes)| {
            // Each programme must name the channel it came with (any ASCII case, or nothing); one that
            // names another channel is not this channel's and is dropped.
            let ids = ChannelIds::single(&channel.id.as_str().intern());
            let programmes: Vec<EpgProgramme> =
                programmes.into_iter().filter_map(|programme| programme_from_wit(programme, &ids)).collect();
            (PluginEpgChannel { id: channel.id, names: channel.names, icon: channel.icon }, programmes)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PlaylistItemType, XtreamCluster};
    use tuliprox_plugin_api::bindings::tuliprox::plugin::types::{Cluster, ItemType};

    fn channel(name: &str) -> Channel {
        Channel {
            handle: 99,
            name: name.to_string(),
            title: format!("{name} title"),
            group: "News".to_string(),
            url: format!("http://host/{name}"),
            epg_id: Some("epg.one".to_string()),
            logo: "http://host/logo.png".to_string(),
            chno: Some(4),
            item_type: ItemType::Live,
            cluster: Cluster::Live,
            input_name: "ignored".to_string(),
            input_stream_id: String::new(),
            extra: Vec::new(),
        }
    }

    #[test]
    fn a_call_may_return_at_most_a_million_channels() {
        assert!(check_channels("fetch-playlist", MAX_RETURNED_CHANNELS).is_ok());
        let err = check_channels("fetch-epg", MAX_RETURNED_CHANNELS + 1).unwrap_err();
        assert!(matches!(err, CallError::Trap(ref message) if message.contains("fetch-epg returned 1000001")), "{err}");
    }

    #[test]
    fn items_carry_the_plugin_content_and_host_issued_fields() {
        let items = channels_to_items("my-input", vec![channel("a"), channel("b")]);
        assert_eq!(items.len(), 2);
        let header = &items[1].header;
        assert_eq!(&*header.input_name, "my-input");
        assert_eq!(header.source_ordinal, 2);
        assert_eq!(&*header.name, "b");
        assert_eq!(&*header.title, "b title");
        assert_eq!(&*header.group, "News");
        assert_eq!(&*header.url, "http://host/b");
        assert_eq!(header.epg_channel_id.as_deref(), Some("epg.one"));
        assert_eq!(header.chno, 4);
        assert_eq!(header.item_type, PlaylistItemType::Live);
        assert_eq!(header.xtream_cluster, XtreamCluster::Live);
        // The handle and the plugin's idea of the input name are not used.
        assert_ne!(&*header.input_name, "ignored");
        assert_eq!(items[0].header.source_ordinal, 1);
    }

    #[test]
    fn provider_id_prefers_stream_id_then_extra_then_a_stable_hash() {
        let mut with_stream_id = channel("a");
        with_stream_id.input_stream_id = " s-1 ".to_string();
        with_stream_id.extra = vec![(PROVIDER_ID_KEY.to_string(), "p-1".to_string())];
        assert_eq!(provider_id("in", &with_stream_id), "s-1");

        let mut with_extra = channel("a");
        with_extra.extra = vec![(PROVIDER_ID_KEY.to_string(), "p-1".to_string())];
        assert_eq!(provider_id("in", &with_extra), "p-1");

        let bare = channel("a");
        assert_eq!(provider_id("in", &bare), provider_id("in", &bare));
        assert_ne!(provider_id("in", &bare), provider_id("other", &bare));
        assert_ne!(provider_id("in", &bare), provider_id("in", &channel("b")));
    }

    #[test]
    fn empty_name_title_group_and_blank_epg_id_get_host_defaults() {
        let mut sparse = channel("a");
        sparse.name = String::new();
        sparse.group = "  ".to_string();
        sparse.epg_id = Some(" ".to_string());
        sparse.chno = None;
        let items = channels_to_items("in", vec![sparse]);
        let header = &items[0].header;
        assert_eq!(&*header.name, "a title");
        assert_eq!(&*header.group, DEFAULT_GROUP);
        assert!(header.epg_channel_id.is_none());
        assert_eq!(header.chno, 0);

        let mut untitled = channel("a");
        untitled.title = String::new();
        let items = channels_to_items("in", vec![untitled]);
        assert_eq!(&*items[0].header.title, "a");
    }

    #[test]
    fn the_wit_request_carries_the_input_as_configured() {
        let options = vec![("k".to_string(), "v".to_string())];
        let request = PluginInputRequest {
            input_name: "in",
            url: "http://host/list",
            username: Some("u"),
            password: None,
            options: &options,
        };
        let wit = wit_request(&request);
        assert_eq!(wit.input_name, "in");
        assert_eq!(wit.url, "http://host/list");
        assert_eq!(wit.username.as_deref(), Some("u"));
        assert_eq!(wit.password, None);
        assert_eq!(wit.options, options);
    }
}
