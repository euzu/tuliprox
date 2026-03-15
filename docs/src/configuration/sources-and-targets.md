# Sources And Targets

`source.yml` defines upstream inputs, provider aliases and the targets Tuliprox publishes to clients.

## Top-level entries

- `templates`
- `inputs`
- `sources`

For new setups, prefer global `template_path` in `config.yml`, but inline `templates` remain supported.

## `templates`

Templates reduce repeated regular expressions and can reference other templates via `!name!`.

Example:

```yaml
templates:
  - name: delimiter
    value: '[\s_-]*'
  - name: quality
    value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'
```

## `inputs`

Each input can define:

- `name`
- `type`: `m3u`, `xtream`, `library`, `m3u_batch`, `xtream_batch`
- `enabled`
- `persist`
- `url`
- `epg`
- `headers`
- `method`
- `username`
- `password`
- `panel_api`
- `cache_duration`
- `exp_date`
- `options`
- `aliases`
- `staged`

### URL schemes

Supported input URL schemes:

- `http://` / `https://`
- `file://`
- `provider://<provider_name>/...`
- `batch://...`

If an `m3u` or `xtream` input uses a `batch://` URL, Tuliprox treats it as `m3u_batch` or `xtream_batch`.

### Common input options

Important `options` fields include:

- `xtream_skip_live`
- `xtream_skip_vod`
- `xtream_skip_series`
- `xtream_live_stream_without_extension`
- `xtream_live_stream_use_prefix`
- `disable_hls_streaming`
- `resolve_tmdb`
- `probe_stream`
- `resolve_background`
- `resolve_series`
- `resolve_vod`
- `probe_series`
- `probe_vod`
- `probe_live`
- `probe_live_interval_hours`
- `resolve_delay`
- `probe_delay`

Example Xtream input:

```yaml
inputs:
  - name: my_provider
    type: xtream
    url: http://provider.example:8080
    username: user
    password: secret
    options:
      resolve_tmdb: true
      probe_stream: true
      resolve_series: true
      resolve_vod: true
```

### EPG configuration

Inputs can define multiple XMLTV sources with priorities and smart matching:

```yaml
epg:
  sources:
    - url: auto
      priority: -2
      logo_override: true
    - url: http://localhost:3001/xmltv.php?epg_id=1
      priority: -1
  smart_match:
    enabled: true
    fuzzy_matching: true
    match_threshold: 80
    best_match_threshold: 99
```

### Aliases

Aliases let one logical provider input expose multiple credentials.

```yaml
inputs:
  - type: xtream
    name: my_provider
    url: http://provider.net
    username: primary
    password: secret1
    aliases:
      - name: my_provider_2
        url: http://provider.net
        username: secondary
        password: secret2
        max_connections: 2
```

### Batch inputs

Batch inputs load aliases from CSV files.

`xtream_batch` CSV fields:

- `name`
- `username`
- `password`
- `url`
- `max_connections`
- `priority`
- `exp_date`

`m3u_batch` CSV fields:

- `url`
- `max_connections`
- `priority`

### `panel_api`

Tuliprox can provision or renew provider accounts through a panel API.
Supported operations include:

- `account_info`
- `client_info`
- `client_new`
- `client_renew`
- `client_adult_content`

Important controls:

- `alias_pool.size.min`
- `alias_pool.size.max`
- `alias_pool.remove_expired`
- `provisioning.timeout_sec`
- `provisioning.method`
- `provisioning.probe_interval_sec`
- `provisioning.cooldown_sec`
- `provisioning.offset`

Example:

```yaml
panel_api:
  url: https://panel.example/api.php
  api_key: "1234567890"
  provisioning:
    timeout_sec: 65
    method: GET
    probe_interval_sec: 10
    cooldown_sec: 120
    offset: 12h
```

### Staged inputs

`staged` lets Tuliprox read playlist data from another source during updates while still using the main input for streaming and details.

Important fields:

- `enabled`
- `type`
- `url`
- `headers`
- `method`
- `username`
- `password`
- `live_source`
- `vod_source`
- `series_source`

## `sources`

Each `source` contains:

- `inputs`
- `targets`

The `inputs` list references input names from the top-level `inputs` section.

## `targets`

Each target can define:

- `enabled`
- `name`
- `sort`
- `output`
- `processing_order`
- `options`
- `filter`
- `rename`
- `mapping`
- `favourites`
- `watch`
- `use_memory_cache`

### `output`

Supported output types:

- `xtream`
- `m3u`
- `strm`
- `hdhomerun`

Important output-specific fields:

`xtream`

- `skip_live_direct_source`
- `skip_video_direct_source`
- `skip_series_direct_source`
- `update_strategy`
- `trakt`
- `filter`

`m3u`

- `filename`
- `include_type_in_url`
- `mask_redirect_url`
- `filter`

`strm`

- `directory`
- `username`
- `underscore_whitespace`
- `cleanup`
- `style`
- `flat`
- `strm_props`
- `add_quality_to_filename`
- `filter`

`hdhomerun`

- `device`
- `username`
- `use_output`

### Target `options`

- `ignore_logo`
- `share_live_streams`
- `remove_duplicates`
- `force_redirect`

If `share_live_streams` is enabled, each shared live channel consumes buffer memory even when several clients share the same upstream stream.

### `sort`

Sorting consists of ordered rules with:

- `target`: `group` or `channel`
- `field`
- `filter`
- `order`
- `sequence`

Example:

```yaml
sort:
  rules:
    - target: group
      order: asc
      filter: Group ~ ".*"
      field: group
      sequence:
        - '^Freetv'
        - '^Shopping'
        - '^Entertainment'
```

### `processing_order`

Controls the order of Filter, Rename and Map.
Valid values:

- `frm`
- `fmr`
- `rfm`
- `rmf`
- `mfr`
- `mrf`

### `filter`

Filters support `NOT`, `AND`, `OR`, regex matches and `Type` comparisons.
Fields include:

- `Group`
- `Title`
- `Name`
- `Caption`
- `Url`
- `Genre`
- `Input`
- `Type`

Example:

```text
((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")
```

### `rename`

Rename rules contain:

- `field`
- `pattern`
- `new_name`

Example:

```yaml
rename:
  - field: group
    pattern: '^DE(.*)'
    new_name: '1. DE$1'
```

### `mapping`

Targets can reference mapping IDs defined in `mapping.yml`.

### `favourites`

Allows explicit favorite groups after mapping and resolution.

Example:

```yaml
favourites:
  - cluster: series
    group: "My Favourites"
    filter: 'Name ~ "Cinema"'
    match_as_ascii: true
```

### `watch`

Watches final group names and emits notifications when those groups change.

```yaml
watch:
  - 'FR - Movies \(202[34]\)'
  - 'FR - Series'
```

## Example `source.yml`

```yaml
templates:
  - name: ALL_CHAN
    value: 'Group ~ ".*"'
inputs:
  - type: xtream
    name: my_provider
    url: http://provider.example:8080
    username: user
    password: secret
sources:
  - inputs:
      - my_provider
    targets:
      - name: all_channels
        output:
          - type: xtream
          - type: m3u
        options:
          ignore_logo: false
          share_live_streams: true
        filter: "!ALL_CHAN!"
```
