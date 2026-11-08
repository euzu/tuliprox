# Managed channels in existing inputs

A plugin can add live channels to an existing M3U or Xtream input without a separate
`type: plugin` input. This supports integrations that maintain event or persistent team
channels. It is a processing overlay: it changes what targets receive, not the provider's
playlist or the raw provider cache.

## Manifest and SDK

Request the `playlist_write` capability and declare `playlist.managed-channels`:

```yaml
capabilities:
  playlist_write: true
hooks:
  - playlist.managed-channels
```

Implement `ManagedChannelHooks::reconcile_input(input_name)` inside a
`#[tuliprox_plugin]` module. The additive WIT interface is `managed-channel-hooks` and
its standalone world is `managed-channel-plugin`. Existing interfaces are unchanged.

The return value is the **complete** `Vec<ManagedChannel>` owned by this plugin for the
named input. Each entry has a stable `id: String` and `value: Channel` (available in the
SDK prelude). The host uses the returned name, title, group, URL, EPG ID, logo and channel
number. `handle`, `input_name`, `input_stream_id` and `extra` cannot override ownership.

```rust,ignore
impl ManagedChannelHooks for Bridge {
    fn reconcile_input(input_name: &str) -> Result<Vec<ManagedChannel>, String> {
        let settings = Settings::current()?;
        if input_name != settings.append_to_input {
            return Ok(vec![]);
        }
        // Read the complete list maintained by HTTP handlers, e.g. from plugin KV.
        // Every entry has a stable event/team id and its current playback URL.
        load_managed_channels()
    }
}
```

`append_to_input` and `load_managed_channels` are the integration plugin's settings and
logic, not built-in SDK functions. Selecting a destination happens in the plugin; the
host calls the hook for each eligible input. Return `Ok([])` for unselected inputs.
When changing the destination, return an empty list for the old input to remove its
contribution on that input's next successful refresh.

## Reconciliation and ownership

- A new ID adds a channel. Returning an existing ID updates it. Omitting an ID removes
  that channel from the next processed view. `Ok([])` deliberately removes all channels
  owned by this plugin in that input.
- IDs must be unique within a snapshot, nonblank, trimmed, at most 256 bytes and contain
  no control characters. Identity is derived from plugin ID, input name and managed ID.
  Name, URL, channel number and return order do not determine identity. Tuliprox also
  includes the stream type in runtime identity; keep `item_type` stable for a managed ID.
- Two plugins can use the same managed ID without colliding. A plugin cannot name a
  provider channel or another plugin's channel for deletion. Ordinary filtering can
  still filter any channel during target processing.
- For now entries must be live (`live`, `live-unknown`, `live-hls`, `live-dash`) with the
  `live` cluster and an absolute HTTP(S) playback URL. VOD, series and local-file channels
  are rejected. Group/name defaults follow the input-provider conventions.
- All entries are validated before any replacement. Duplicate IDs, invalid URLs, a
  guest error, trap, timeout, resource exhaustion or failed snapshot save preserve the
  previous successful contribution. A plugin with no successful snapshot contributes
  nothing on failure. Failures still appear in plugin stats/logs and count toward the
  existing circuit breaker; a tripped breaker serves the last-good snapshot until reset
  or reload. `abort-target` does not override last-good fallback for this hook.

## Processing order and storage

The merge runs after provider loading, staged overlays, and `playlist.loaded` inventory
hooks, but before target filtering/mapping/output. The inventory hook therefore sees
provider data without feeding the plugin's own generated output back into discovery.
Staged inputs are merged through their parent and are not independently overlaid.

It also runs when the provider download is cached. Managed channels follow provider
channels in source order, then participate in normal target processing, sorting, EPG
matching and output. Input cluster exclusions still apply. Targets consuming the
existing input receive its managed channels subject to their filters. A plugin with
`playlist_write` does not bypass target rules.

The raw provider snapshot is never rewritten with managed channels. Disk-backed
providers stay lazy; only the managed contribution is held in memory. Local group IDs
are reassigned at merge time to avoid collisions with provider category IDs.

The host persists successful snapshots under
`<plugins data_dir>/.managed-channels/<plugin-id>/<sha256(input-name)>.json`, atomically replacing
one complete snapshot. This store is separate from guest-accessible KV, so a plugin
cannot overwrite the fallback while preparing a replacement. Snapshots survive host
restart and plugin reload. Corrupt/unreadable snapshot files are reported as input
errors rather than interpreted as an intentional deletion.

Disabling, unapproving or removing a plugin stops its contribution on subsequent
processing; its snapshots are retained for re-enabling. Uninstall with `purge_kv=true`
also deletes its managed snapshots. A live target remains unchanged until refreshed.

Limits: 10,000 channels and 16 MiB serialized snapshot per plugin/input. Normal hook
memory, instance and execution limits apply. Calls for the same plugin/input are
serialized; independent inputs and plugins can reconcile concurrently.

## Bridge workflow

```text
Existing M3U/XC input → provider inventory hook → bridge HTTP API ↔ Teamarr
          │                                                   │
          │                                      stored managed channel list
          │                                                   │
          └──────────── merge managed-channel hook ◄───────────┘
                                      │
                              existing target/output
```

HTTP handlers update the integration plugin's list. A playlist refresh invokes the
managed hook and rebuilds targets. The hook does not itself trigger that refresh, fetch
Teamarr XMLTV, or implement stream fallback. Those remain integration responsibilities.
This support does not install a Teamarr/Dispatcharr compatibility plugin.
