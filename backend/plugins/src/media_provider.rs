//! `type: plugin` media-server inputs: `fetch-catalog` of the plugin an input
//! names, under the provider deadline.
//!
//! Calls `fetch-catalog` page by page until `next-cursor` is None.
//! Enforces:
//! - Per-page provider deadline (`provider_exec`)
//! - Repeating cursor detection and max pages cap (10,000)
//! - Max total items cap (1,000,000)
//! - Complete-before-publish: converts to `MediaServerCatalogSnapshot` and maps
//!   via `media_server_catalog_snapshot_to_playlist`.

use crate::{
    convert_media::{convert_media_catalog, MAX_MEDIA_ITEMS},
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
};
use shared::model::{HookCtx, PlaylistGroup, PluginHook, PluginHookStats, PluginInputRequest, PluginRunReport};
use std::{collections::HashSet, sync::Arc, time::Instant};
use tuliprox_media_server::{media_server_catalog_snapshot_to_playlist, MediaServerCatalogSnapshot};
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::media_provider_v1_1::{MediaCatalogPage, MediaRequest},
    tuliprox::plugin::media_types_v1_1::MediaCatalog,
};

pub const MAX_CATALOG_PAGES: usize = 10_000;

fn provider<'d>(
    dispatcher: &'d HookDispatcher<'_>,
    plugin: &str,
) -> Result<(&'d Arc<PluginRecord>, &'d LoadedPlugin), String> {
    dispatcher
        .named(PluginHook::MediaFetch, plugin)
        .ok_or_else(|| format!("plugin {plugin} is not enabled or does not provide a media provider"))
}

fn wit_media_request(request: &PluginInputRequest<'_>, cursor: Option<String>) -> MediaRequest {
    MediaRequest {
        input_name: request.input_name.to_string(),
        url: request.url.to_string(),
        username: request.username.map(ToString::to_string),
        password: request.password.map(ToString::to_string),
        options: request.options.to_vec(),
        refresh: request.refresh,
        last_success_at: request.last_success_at,
        cursor,
    }
}

fn close<T>(
    plugin: &PluginRecord,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    kept: usize,
    started: Instant,
    outcome: &Result<T, CallError>,
) {
    let mut stats = PluginHookStats::new(&plugin.id, PluginHook::MediaFetch);
    stats.kept = count(kept);
    let mut report = PluginRunReport::default();
    let _ = HookDispatcher::settle(
        plugin,
        loaded,
        PluginHook::MediaFetch,
        ctx,
        stats,
        started,
        outcome.as_ref().map(|_| ()),
        &mut report,
    );
}

/// Fetch catalog snapshot from the plugin, paging until next-cursor is None.
pub(crate) async fn fetch_media_catalog_snapshot(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginInputRequest<'_>,
) -> Result<MediaServerCatalogSnapshot, String> {
    let (record, loaded) = provider(dispatcher, plugin)?;
    let started = Instant::now();
    let outcome = call_fetch_media_catalog(dispatcher, loaded, &record.id, request).await;
    let kept = outcome.as_ref().map_or(0, MediaServerCatalogSnapshot::item_count);
    close(record, loaded, ctx, kept, started, &outcome);
    outcome.map_err(|err| err.to_string())
}

/// Fetch catalog and map to playlist groups.
pub(crate) async fn fetch_media_catalog(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginInputRequest<'_>,
) -> Result<Vec<PlaylistGroup>, String> {
    let snapshot = fetch_media_catalog_snapshot(dispatcher, plugin, ctx, request).await?;
    Ok(media_server_catalog_snapshot_to_playlist(&snapshot))
}

async fn call_one_page(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    request: &PluginInputRequest<'_>,
    cursor: Option<String>,
) -> Result<MediaCatalogPage, CallError> {
    let indices = loaded
        .media_provider
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export media-provider-v1-1".into()))?;
    let exec = loaded.prepared.limits().provider_exec(dispatcher.runtime().config());
    let mut live = dispatcher.open_with(loaded, exec).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();
    let wit_req = wit_media_request(request, cursor);
    let page = call_with_deadline(limits, guest.call_fetch_catalog(live.armed(), &wit_req)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    let page = page?.map_err(|err| CallError::Trap(format!("fetch-catalog returned err: {err}")))?;
    Ok(page)
}

fn catalog_item_count(c: &MediaCatalog) -> usize {
    c.movies.len() + c.series.len() + c.seasons.len() + c.episodes.len()
}

async fn call_fetch_media_catalog(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    plugin_id: &str,
    request: &PluginInputRequest<'_>,
) -> Result<MediaServerCatalogSnapshot, CallError> {
    let mut catalogs = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut total_items: usize = 0;

    for page_idx in 0..MAX_CATALOG_PAGES {
        if let Some(ref c) = cursor {
            if !seen_cursors.insert(c.clone()) {
                return Err(CallError::Trap(format!("fetch-catalog repeating cursor loop detected: {c}")));
            }
        }

        let page = call_one_page(dispatcher, loaded, request, cursor.take()).await?;
        let page_items = catalog_item_count(&page.catalog);
        total_items = total_items.saturating_add(page_items);
        if total_items > MAX_MEDIA_ITEMS {
            return Err(CallError::Trap(format!("fetch-catalog returned {total_items} items (cap {MAX_MEDIA_ITEMS})")));
        }

        catalogs.push(page.catalog);

        let page_num = page_idx + 1;
        request.emit_progress(request.input_name, &format!("Fetched catalog page {page_num} ({total_items} items)"));

        match page.next_cursor {
            Some(next) if !next.trim().is_empty() => {
                cursor = Some(next);
            }
            _ => break,
        }

        if page_idx == MAX_CATALOG_PAGES - 1 && cursor.is_some() {
            return Err(CallError::Trap(format!("fetch-catalog exceeded maximum pages cap of {MAX_CATALOG_PAGES}")));
        }
    }

    convert_media_catalog(request.input_name, plugin_id, catalogs).map_err(CallError::Trap)
}
