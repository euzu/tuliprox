# Hook Reference

Every supported hook id listed in a manifest's `hooks:` must have a matching export in the component, and the other way
round. `epg.resolve-conflict` is reserved but unsupported; a manifest that declares it is rejected.
The ids are the table `HookId` in `backend/plugins/src/manifest/hook.rs`. Ids are case sensitive. The six stage ids need
the `playlist.` prefix: the bare form `after-filter` is refused with a message that names the right spelling.

The following apply to pipeline hooks:

- **Fail-open.** A failing call leaves the data as the previous plugin left it. The failure is counted on the plugin's
  circuit breaker. Only `resources.fail_mode: abort-target` changes that, and only for the playlist hooks that go through
  the dispatcher (`backend/plugins/src/dispatch.rs`) and the strategy hooks. The EPG and metadata hooks are always
  fail-open (`backend/plugins/src/epg_hooks.rs`). See [Overview](./overview.md#fail-open).
- **Chaining.** When several plugins export the same hook, they run in `priority` order (default 100, lower first),
  then by id. Each plugin sees what the one before it left. Its results apply only when its whole run succeeded.

Incoming `http.handle` requests are dispatched to one named plugin and fail closed; they do not chain or trip the pipeline breaker. See [Incoming HTTP routes](./http-routes.md).

## Hook table

Each row gives the WIT interface the component exports, the SDK trait method that implements it with its input and
output, the extra requirement in the manifest, and where the host calls the hook. The SDK traits are in
`plugins/plugin-sdk/src/hooks.rs` and the WIT is in `plugins/plugin-api/wit/tuliprox-plugin.wit`. File names in the last column are
relative to `backend/plugins/src/`, `backend/processing/src/` or `backend/app/src/api/endpoints/`.

| Hook id | Interface | SDK method: input -> output | Also requires | When it runs |
| --- | --- | --- | --- | --- |
| `playlist.managed-channels` | `managed-channel-hooks` | `reconcile(request)` -> complete managed channel list | `playlist_write` | Configured input or target filter point; failures retain per-binding last-good channels ([details](./managed-channels.md)) |
| `http.handle` | `http-routes` | `authenticate(request)` -> identity; `handle(request)` -> HTTP response | `http_routes`, non-empty `routes` | A request to the plugin HTTP namespace (`http_routes.rs`, `plugins_http_api.rs`) |
| `playlist.before-filter` | `playlist-hooks` | `on_stage(stage, target, channels)` -> sparse `ChannelChange` list | none | Before the filter stage of each target (`processor/playlist/transform.rs`) |
| `playlist.after-filter` | `playlist-hooks` | same | none | After the filter stage |
| `playlist.before-rename` | `playlist-hooks` | same | none | Before the rename stage |
| `playlist.after-rename` | `playlist-hooks` | same | none | After the rename stage |
| `playlist.before-map` | `playlist-hooks` | same | none | Before the map stage |
| `playlist.after-map` | `playlist-hooks` | same | none | After the map stage |
| `playlist.loaded` | `source-hooks` | `on_playlist_loaded(input_name, channels)` -> sparse change list | none | After an input's playlist is read, before target processing (`processor/playlist/ingest/acquisition.rs`) |
| `playlist.after-sort` | `sort-hooks` | `after_sort(target, channels)` -> handles in the new order | none | After a target is sorted (`processor/playlist/target.rs`) |
| `playlist.enrich-metadata` | `metadata-hooks` | `enrich_metadata(item, metadata_json)` -> amended JSON or `None` | none | While VOD or series metadata is updated, before it is saved (`processor/xtream_vod.rs`) |
| `playlist.sort-key` | `sort-key-hooks` | `sort_key(target, channels)` -> one key per channel | none | Only for a target with `sort.strategy: plugin:<id>` |
| `playlist.assign-chno` | `chno-hooks` | `assign_channel_numbers(target, channels)` -> `(handle, chno)` pairs | none | Only for a target with `chno: plugin:<id>` |
| `playlist.resolve-duplicates` | `dedupe-hooks` | `resolve_duplicates(target, group)` -> handles to keep, best first | none | Only for a target with `options.deduplicate.keep: plugin:<id>` |
| `playlist.merge-groups` | `group-merge-hooks` | `merge_groups(target, groups)` -> canonical name and its member groups | none | Only for a target with `group_merge: plugin:<id>` |
| `filter.eval-fn` | `filter-hooks` | `eval_filter_batch(name, channels, args)` -> one `bool` per channel | none | A filter term `plugin.<id>.<fn>(...)` |
| `stream.inject-headers` | `stream-hooks` | `inject_upstream_headers(req)` -> extra header pairs | capability `stream_headers` | Once when a client stream session is set up (`header_inject.rs`) |
| `epg.loaded` | `epg-load-hooks` | `on_epg_loaded(source, channels)` -> changed EPG channels | none | When an EPG source's channel list is parsed (`processor/epg.rs`) |
| `epg.transform-programmes` | `epg-transform-hooks` | `transform_programmes(source, programmes)` -> programmes to keep | capability `epg_write` | Per bounded batch while an EPG source is built |
| `epg.enrich-programmes` | `epg-enrich-hooks` | `enrich_programmes(channel_id, programmes)` -> programmes | capability `epg_write` | For one channel's programmes |
| `epg.match-channel` | `epg-match-hooks` | `match_channels(requests)` -> one `Option<String>` per request | none | For channels the built-in matcher left without a guide id |
| `epg.resolve-conflict` | `epg-conflict-hooks` | `resolve_conflict(conflict)` -> candidate index or `None` | none | **Unsupported.** Manifest validation rejects this hook until its host integration is implemented |
| `events.on-event` | `event-hooks` | `on_event(ev)` -> `()` | a non-empty `events:` list | Each bus event that `events:` names, queued (`events.rs`) |
| `metrics.collect` | `metrics-hooks` | `collect_metrics()` -> list of metrics | none | Polled for the dashboard, at most every 10 seconds (`metrics.rs`) |
| `notifications.send` | `notification-hooks` | `send_notification(msg)` -> `Ok` or a `DeliveryError` | capability `notifications` | When messaging delivers to the plugin's channel (`notify.rs`) |
| `input.fetch` | `input-provider` | `fetch_playlist(req)` -> channels, `fetch_epg(req)` -> channels with programmes | `provides.input_type: true` | When an input with `type: plugin` is fetched (`input_provider.rs`) |
| `output.render` | `output-generator` | `begin_output`, `render_batch`, `finish_output` -> bytes | `provides.output_format` | When a target output with `type: plugin` is written (`output_provider.rs`) |
| `rpc.call` | `rpc` | `call(method, payload)` -> JSON string | a non-empty `ui.rpc_exports` | `POST /api/v1/plugins/{id}/rpc/{method}` and MCP tools (`plugins_rpc_api.rs`). Not a pipeline hook |
| `tasks.run` | `tasks` | `run(ctx)` -> summary string | a non-empty `tasks:` list | The scheduler and the manual run route (`tasks.rs`) |
| `settings.migrate` | `settings-migration` | `SettingsMigration::migrate(from_version, settings)` -> settings JSON for the current schema version | a settings schema with a `version` (see [Settings](settings.md)) | Once after the plugins reload, when the stored `settings_version` is behind the schema's (`migration.rs`). Not a pipeline hook |

`HookId::pipeline_hook()` in `backend/plugins/src/manifest/hook.rs` maps supported pipeline ids to host hooks, which the
modules named above dispatch. The manifest validator rejects `epg.resolve-conflict`; `rpc.call` and `settings.migrate`
are dispatched outside the pipeline.

Notes on single hooks:

- `playlist.before-*` and `playlist.after-*` all use `playlist-hooks`. The component gets the stage as an argument, and
  only stages the manifest lists arrive. Ignore the others, as `plugins/examples/template/src/lib.rs` does.
- `playlist.enrich-metadata` is non-fatal, like a failed TMDB lookup.
- `epg.match-channel` is plural and batched. Each request carries the channel and a shortlist of up to 5 EPG channels.
  It is asked twice at most per name and refresh. First over each guide's channel list, before its programmes are
  read: the shortlist is the guide channels whose own names share a matcher bucket (sound, first three letters, name
  without country) with the playlist name, so a pick keeps its programmes even when no other playlist channel uses
  that guide channel. Then, after all guides are read, for channels that still have no guide id and were not asked
  yet (for example a stale id of their own). A name that shares no bucket with any guide name gets no request.
- `stream.inject-headers` runs under a 50 ms clamp for the whole step, with a cache per plugin, user and channel. A
  plugin that traps, errors or is late adds nothing. The host drops connection-control headers: `host`,
  `content-length`, `transfer-encoding`, `connection`, `keep-alive`, `te`, `trailer`, `upgrade` and any `proxy-*`
  (`backend/plugins/src/header_inject.rs`). `plugins.stream_header_ttl` (default `60s`) sets how long a result is
  reused.

## Scope

`scope:` limits which items a hook receives. A plugin whose scope matches nothing is never instantiated.

- `scope.groups` is a list of regular expressions, matched against the group a channel has when the hook runs. Each
  pattern is checked as a regex when the manifest is loaded.
- `scope.item_types` is a list of playlist item types, written in snake case, for example `live` or `series_info`.
  `movie` is accepted as the name of the video type.

Scope applies to the playlist item hooks: the six stages, `playlist.loaded`, `playlist.after-sort` and
`playlist.assign-chno`. The three strategies that need a decision for every item (`sort-key`, `resolve-duplicates`,
`merge-groups`) are sent everything on their target. The EPG hooks do not read `scope`.

From `plugins/examples/sports-enricher/manifest.yml`:

```yaml
scope:
  groups: ["(?i)sport|football|soccer|basketball|american football|ice hockey|baseball|tennis|rugby|cricket|golf|motorsport|combat"]
```

The comment above that block in the manifest explains why the pattern also names the groups the plugin itself
creates: `after-sort` sees those groups, not the provider's.

## Batching

The host never sends a whole large playlist in one call. Items go to a plugin in batches of `plugins.batch_size`
(default 500, at most 10000). Each batch issues handles `0..len`, so a handle is valid for one call only. The plugin
answers with sparse changes:

- `drop(handle)` removes an item.
- `modified(channel)` rewrites the item's writable fields: `name`, `title`, `group`, `url`, `logo`, `epg-id` and
  `chno`. The other fields (`handle`, `item-type`, `cluster`, `input-name`, `input-stream-id`, `extra`) are
  host-owned and ignored on return. A blank `epg-id` means none.
- An item the plugin does not mention is kept.

A handle that was never issued, or that was already answered, is ignored and counted. The first answer for a handle
wins. The sparse merge applies only after every batch succeeded.

Source: the module documentation of `backend/plugins/src/dispatch.rs`, and `apply_channel` in
`backend/plugins/src/convert.rs`.

## Opt-in strategies

Four hooks are strategies. A target uses one only when its config names the plugin as `plugin:<id>`:

| Strategy hook | Config key in the target | Effect |
| --- | --- | --- |
| `playlist.sort-key` | `sort.strategy` | The host stable-sorts the channels of each group by the returned key, after the `rules` |
| `playlist.resolve-duplicates` | `options.deduplicate.keep` | The plugin picks the survivors of each duplicate group |
| `playlist.assign-chno` | `chno` | The plugin numbers channels. Unlisted channels keep the host number, and `0` is ignored |
| `playlist.merge-groups` | `group_merge` | The plugin returns a canonical group name and the groups merged under it |

Source: `shared/src/model/config/sort.rs`, `shared/src/model/config/target.rs`,
`shared/src/model/config/target/options.rs` and `shared/src/model/config/target/plugin_ref.rs`. The reference is
checked for syntax when the config is read, and against the loaded plugins right after plugin load.

A plugin that leaves a decision open gets the host default. For `resolve-duplicates` the default is `best_quality`, and
an empty list means "host default" for that group. A failing run also gets the host default, unless the plugin runs
under `fail_mode: abort-target`.

## After-sort

`playlist.after-sort` returns an order. The host cuts the view into consecutive batches of at most `batch_size`
channels and permutes each batch within the positions it already occupies (`backend/plugins/src/strategy.rs`). A
channel never crosses into another batch. A view that fits one batch is ordered completely by the plugin. Handles the
plugin does not list keep their relative order after the listed ones.

## Miss-only matching

For `epg.match-channel`, the host chains plugins so a later plugin is asked only about the channels that earlier plugins
left open (`backend/plugins/src/epg_hooks.rs`). A plugin therefore sees only the channels it has to decide. The answer
is index-aligned with the requests: the winning EPG channel id, or `None`.

The host only sends channels that have a shortlist, and the shortlist comes from guide channels that kept programmes.
`plugins/examples/hf-epg-matcher/demo/source.yml` sets the built-in matcher to exact names only, so that two misnamed
channels are left for the plugin. `plugins/examples/hf-epg-matcher/demo/CHECKS.md` explains the setup.

The request and the EPG types are `match-request`, `epg-channel` and `epg-programme` in
`plugins/plugin-api/wit/tuliprox-plugin.wit`. Their Rust forms are in the SDK rustdoc (`cargo doc -p tuliprox-plugin-sdk`).

`epg.transform-programmes` and `epg.enrich-programmes` need capability `epg_write`. `epg.transform-programmes` returns
the batch to keep, in order, and may drop, rewrite or add programmes.
