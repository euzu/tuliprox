# Managed channels

`ManagedChannelHooks` supplies plugin-owned live channels to an existing input or a selected
target. There is one hook, one SDK API and no separate input provider or insertion package.
The hook is unreleased: its original input-name-only signature was updated before release.
Rebuild local managed-channel plugins against this contract.

## Select the destination

Configure bindings on the input or target in `source.yml`. The source editor exposes the
same **Managed channels** controls on those blocks, including the target insertion point.
Only enabled plugins providing `playlist.managed-channels` can be added in the editor;
a configured but unavailable plugin can still be removed. Duplicate references within one
destination are rejected, including attempts to bind one plugin before and after filtering.

```yaml
inputs:
  - name: provider
    type: m3u
    url: https://provider.example/playlist.m3u
    managed_channels:
      - "plugin:sports-bridge"

sources:
  - inputs: [provider]
    targets:
      - name: Main TV
        filter: 'Group ~ "News"'
        managed_channels:
          - plugin: "plugin:sports-bridge"
            stage: after_filter
        output:
          - type: m3u
            filename: main.m3u
```

Each reference is the string `plugin:<id>`. Input bindings are a list of those references.
Target bindings are objects with `plugin` and optional `stage` (default `after_filter`).
The example deliberately shows both placements; normally select one for the integration.

| Placement | Result |
| --- | --- |
| Input | Merge after provider inventory hooks, before all target processing. Every target using that input can receive the channels. |
| Target `before_filter` | Only this target receives the channels; its processing filter evaluates them. |
| Target `after_filter` (default) | Only this target receives the channels; they bypass its processing filter. |

The target point follows the Filter stage in its configured `processing_order`. Only
remaining rename/map stages run on newly inserted channels; use the default filter-first
order when those stages should process them. Sorting, numbering, EPG and output processing
still run. Persist/output filters and bouquet restrictions still apply. Input cluster
exclusions cannot be bypassed. Staged overlays cannot have independent input bindings;
attach a binding to the provider parent or target instead.

Bindings are reconciled once per destination before its processing, with insertion delayed
to the configured point. A target binding is not called once per source input. The existing
`playlist.after-filter` inventory hook runs before after-filter managed channels enter,
so it does not discover the integration's own generated output. The managed request contains
allowed input names, not a copy of the filtered provider inventory; inventory and external
integration state remain the plugin's responsibility.

## Plugin contract

```yaml
capabilities:
  playlist_write: true
hooks: [playlist.managed-channels]
```

The WIT interface is `managed-channel-hooks`, its world is `managed-channel-plugin`, and
its SDK trait is `ManagedChannelHooks`. Implement `reconcile(&ManagedChannelRequest)`.
The SDK prelude includes `ManagedChannel`, `ManagedChannelRequest`, `DestinationKind` and
`InsertionPoint`.

```rust,ignore
impl ManagedChannelHooks for Bridge {
    fn reconcile(req: &ManagedChannelRequest) -> Result<Vec<ManagedChannel>, String> {
        // req.kind: Input or Target
        // req.name: destination input/target name
        // req.point: Input, BeforeFilter or AfterFilter
        // req.inputs: source inputs the host permits for this binding
        load_channels_for_destination(req)
    }
}
```

`load_channels_for_destination` is the integration's own logic, e.g. a lookup of the channel
list maintained by its HTTP handlers. A returned entry contains:

- `id`: stable plugin-owned identity, independent of name, URL and return order.
- `source_input`: **required**, and must match one of `req.inputs`.
- `value`: a `Channel` with name/title/group, playback URL, EPG ID, logo and number.

For an input binding, `req.inputs` contains that input only. For a target binding it contains
the target's available source inputs that allow live channels. The host sets each returned
channel's origin from `source_input`; `value.input_name`, `value.input_stream_id`, `handle`
and `extra` cannot override it. The pipeline routes each channel through that source's
playback configuration, headers, credentials, connection limits and EPG processing.
An out-of-scope origin rejects the entire new snapshot. If an input is removed or excludes
live channels, cached fallback channels using that input are also excluded.

## Ownership, persistence and failures

A successful complete list replaces this plugin's contribution to this binding. New IDs
add channels, existing IDs update them, omitted IDs remove them, and `Ok([])` deliberately
clears the contribution. No reconciliation action can delete provider channels or another
plugin's channels. Normal target filters can still filter them.

Snapshots are isolated by plugin, destination kind/name and insertion point. Changing an
insertion point uses a separate fallback snapshot, while retaining the same channel identity
for the same plugin/destination/source/managed ID. Different targets cannot borrow each other's
fallback. Keep the stream type stable for a managed ID: native runtime identity includes type.

Errors, traps, timeouts, resource exhaustion, invalid results and failed snapshot writes retain
the previous successful contribution. Without a prior success, a failure contributes nothing.
Duplicate IDs, whitespace/control characters in IDs, invalid origins and non-HTTP(S) URLs are
rejected before any replacement. IDs are at most 256 bytes. Only live, live-unknown, live-hls and
live-dash streams in the live cluster are supported.

The host atomically persists snapshots under
`<plugins data_dir>/.managed-channels/<plugin-id>/bindings/<scope-hash>.json`, separately from
plugin KV. Corrupt or unreadable snapshots become processing errors rather than an intentional
deletion. Snapshots survive restart/reload. A failed circuit breaker serves its last-good
contribution until reset/reload; `abort-target` does not disable fallback for this hook.

Removing a binding or disabling/unapproving/removing its plugin stops its contribution on the
next processing run. Snapshots remain for re-enabling; uninstall with `purge_kv=true` deletes
the owner's snapshots. Raw provider caches are never rewritten with managed channels, and
input overlays preserve lazy disk-backed processing.

Limits: 10,000 channels and 16 MiB serialized snapshot per binding. Existing hook memory,
instance and execution limits apply. Concurrent calls for one binding are serialized.

A playlist refresh is required to publish changes. The hook does not trigger refreshes,
implement Teamarr's Dispatcharr API, retrieve XMLTV, or provide ordered stream fallback.
Those belong to the integration plugin.
