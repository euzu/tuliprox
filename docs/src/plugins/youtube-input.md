# YouTube Input Provider

`youtube-input` is an input provider plugin that ingests YouTube channels, playlists, and live streams as native
tuliprox playlist channels. It runs fully sandboxed inside WebAssembly (`wasm32-wasip2`), resolving playable streams
via Invidious REST endpoints with sticky instance failover, accurate live status detection, and KV-cache storage.

## Capabilities

* **`network`**: Fetches YouTube Atom/RSS feeds and queries Invidious REST endpoints.
* **`storage`**: Caches resolved stream URLs in the plugin's KV store using video-scoped keys
  (`yt:str:<video_id>`) with active instance verification and configurable TTL.

## Input Configuration

Register the plugin as an input definition in `source.yml` under `inputs:`:

```yaml
inputs:
  - name: youtube-news
    type: plugin
    plugin: youtube-input
    url: "https://www.youtube.com/@tagesschau"
```

The optional `url` property on an input specifies the target (or comma-separated targets) for that specific input;
entries of `settings.channels` are split the same way. When `url` is omitted, the input processes the targets
configured globally under `settings.channels`. An input without any target fails instead of producing an empty playlist.

The Web UI source editor builds the same entry: add the **Plugin** input, choose `youtube-input` in the plugin field,
and enter the target in `Url`. The picker offers the installed plugins that export the `input.fetch` hook; the plugin's
own settings stay under **Plugins**.

## Where the configuration lives

Everything a fetch needs belongs to the input itself. The plugin has no global settings: its
`ui.input_form` puts these fields into the input's **Plugin** panel, stored as `plugin_config` in
`source.yml`.

| Field | Meaning |
| --- | --- |
| `channels` | Targets for this input: `@handle`, channel id `UC…`, playlist id `PL…`, or a video. Added to the input's `Url`; a target named twice is fetched once |
| `invidious_instances` | Mirrors this input resolves streams through, in failover order. Required: without one the fetch fails and names this field |
| `mode` | `recent_uploads` (newest videos) or `live_only` (only the current live stream) |
| `max_uploads` | Newest uploads per channel, 1 to 15 |
| `cache_ttl_minutes` | How long this input reuses a resolved stream URL, 10 to 360 |

```yaml
inputs:
  - name: youtube-news
    type: plugin
    plugin: youtube-input
    plugin_config:
      channels:
        - "@tagesschau"
        - "https://www.youtube.com/@SkyNews"
      invidious_instances:
        - "https://yewtu.be"
        - "https://invidious.nerdvpn.de"
      mode: recent_uploads
      max_uploads: 10

  - name: youtube-live
    type: plugin
    plugin: youtube-input
    url: "https://www.youtube.com/@tagesschau"
    plugin_config:
      invidious_instances:
        - "https://invidious.nerdvpn.de"
      mode: live_only
      cache_ttl_minutes: 30
```

An input may use `url` (the host field), its own `channels` list, or both. Both accept
comma-separated entries. What stays global is host-side, not a plugin setting: the update policy and
the boot-update budget (`plugins.plugins[].update_policy`, `plugins.auto_update_on_boot`, the budget
on the Config page). Because the plugin ships no settings schema, the Plugin Manager's gear icon
shows a note pointing at the input form.

A list-valued field reaches the plugin as its JSON text (`["https://a","https://b"]`), because the
host passes `plugin_config` as string options. A plain comma-separated string works as well.

Configs written before the move keep working: a `settings:` block on the plugin entry is still read
as the default for anything an input leaves unset, and its `channels` list is used only by an input
that has neither a `url` nor its own `channels`.

### Supported targets

| Form | Example |
| --- | --- |
| Channel handle | `@tagesschau`, `https://www.youtube.com/@tagesschau` |
| Channel ID | `UC5NOEUbkLheQcaaRldYW5GA`, `https://www.youtube.com/channel/UC...` |
| Playlist ID | `PLkahZjV5wKe8w9GC_n2yyhx6Wj-NKzzeE`, `https://www.youtube.com/playlist?list=...` |
| Single video | `dQw4w9WgXcQ`, `https://youtu.be/dQw4w9WgXcQ`, `https://www.youtube.com/live/<id>` |
| Legacy names | `https://www.youtube.com/user/<name>`, `https://www.youtube.com/c/<name>` |
| Live page | `@tagesschau/live`, `https://www.youtube.com/channel/UC.../live`, `https://www.youtube.com/c/<name>/live` |

A legacy name is not a handle: it is resolved to its channel ID through its own youtube.com page, then through the
Invidious `resolveurl` endpoint. A live page target always fetches the channel's running broadcast, whatever `mode`
says.

Anything else — an empty string, a foreign domain, a bare word, a malformed video, channel or playlist ID — is
rejected with a warning and counts as a hard error, so a typo surfaces instead of silently shrinking the playlist.

### How `mode` applies

`recent_uploads` reads the channel's Atom feed (its 15 newest videos) and resolves each entry. A playlist target is
read the same way.

`live_only` asks each channel for its current live broadcast (`/@handle/live`, `/channel/<id>/live`, `/c/<name>/live`)
and keeps the stream only when Invidious reports `liveNow`. Only the page's canonical link names the broadcast, so a
channel that is not live is never confused with a stream it merely recommends. For a playlist target there is no live page, so the playlist's entries
are resolved and everything that is not live is dropped. A channel that is simply not live is not an error; a
channel whose live stream cannot be resolved is.

## Field Reference

Values an input leaves empty keep the plugin's built-in default.

| Field | Default | Description |
| --- | --- | --- |
| `channels` | `[]` | Targets for this input (@handle, channel ID `UC...`, playlist ID `PL...`, or a video). Added to the input's `Url`; one entry may hold a comma-separated list. |
| `invidious_instances` | *(none)* | Invidious mirrors for stream resolution with sticky failover. Required per input; the fetch fails without one. |
| `mode` | `"recent_uploads"` | `recent_uploads` (latest uploads) or `live_only` (active streams only). |
| `max_uploads` | `10` | Uploads to import per channel (1 to 15; RSS provides up to 15). |
| `cache_ttl_minutes` | `120` | How long a resolved stream URL is reused (10 to 360). Active live streams use 10m. |

## Runtime Budget & Stream Proxying

* **Execution Budget:** The plugin enforces a wall-clock deadline budget of 25 seconds per input fetch, well inside
  the host's provider deadline `min(plugins.max_exec × 10, plugins.max_task_exec)`. With the host defaults
  (`max_exec: 5s`, `max_task_exec: 10m`) that is 50 s. Keep `plugins.max_exec` at 3 s or more; below roughly 2.6 s the
  provider deadline is shorter than the plugin's own budget and the host ends the call first.
* **Stream Proxying (`?local=true`):** Invidious stream URLs are queried with `?local=true` so stream delivery is
  proxied through the Invidious instance, preventing client players from encountering 403 Forbidden errors caused by
  IP-bound upstream signatures.
* **Lifetime of the returned URLs:** an MP4 is returned as the instance's `latest_version?id=…&itag=…` link, which
  stays valid because the instance redirects it to the current signed URL. A live HLS manifest URL is signed by
  YouTube and expires after roughly six hours; refresh a `live_only` input more often than that (its cache entry
  already lives only 10 minutes).
* **Health of the returned URLs:** if every target fails (unreachable instances, unparsable feeds, unresolvable
  streams) the input fails and tuliprox keeps the previous playlist. An empty result is only returned when the
  targets were reached and simply had nothing to offer — no live stream right now, or an empty feed.

## Build and install

```sh
make plugin-youtube-input
```

Builds the component for `wasm32-wasip2` and writes the uploadable package to
`target/plugin-youtube-input/youtube-input.tlpx` (manifest, `plugin.wasm`, `forms/input.json` and the locale files).
The wasm target must be installed once: `rustup target add wasm32-wasip2`.

Install it in the Plugin Manager (**Plugins → Install plugin**) by uploading that file, or over the API. This plugin
asks for the `network` and `storage` capabilities, so the approval has to name both:

```sh
PACKAGE=target/plugin-youtube-input/youtube-input.tlpx
API=http://127.0.0.1:8901/api/v1/plugins
REVIEW=$(mktemp)

# 1. Upload: stages the package and returns the review. Nothing is installed yet.
curl -s -F "file=@$PACKAGE" "$API" > "$REVIEW"

# 2. Approve: repeat the reviewed hash and every capability name.
HASH=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["wasmSha256"])' "$REVIEW")
curl -s -X PUT "$API/youtube-input/approve" \
  -H 'Content-Type: application/json' \
  -d "{\"wasmSha256\": \"$HASH\", \"capabilities\": [\"network\", \"storage\"], \"mcp\": false}"

# 3. Enable.
curl -s -X PUT "$API/youtube-input/enable"
```

The three routes need `SystemWrite` when web auth is on. Notes that cost time otherwise:

* The approve keys are camelCase. `wasm_sha256` is refused with `409 … wasm hash mismatch`, which reads like a wrong
  package. The capability list must be exactly the one the review printed.
* Approval does not enable the plugin **unless `config.yml` already has a `plugins.plugins[]` entry for it**: that
  entry defaults to `enabled: true`, so approval switches it straight on. A config entry is also an approval, so an
  install that keeps such an entry never shows `pending_approval`.
* A `source.yml` that names `plugin: youtube-input` while the plugin is not installed is reported at startup and then
  ignored — the server starts, the input is inactive until the plugin is enabled, and every other input and target
  keeps running. The plugin directory itself still has to exist before the server starts
  (``none of the configured plugin directories exist``); `plugins.directory` does not create it.
* The input form of a new package version appears after the host has reloaded the plugin.

After the three calls, `GET $API` lists the plugin with `"state": "enabled"`. The full lifecycle — settings, updates,
rollback, disable and uninstall with `?purge_kv=true` — is in the [Quickstart](./quickstart.md) (steps 6–9) and in
[Packaging](./packaging.md#updating-and-rolling-back).

Other targets that build the plugin directories in this repository: `make plugin-inputs` (every input plugin),
`make plugin-notifiers` (every notifier), `make plugins` (all of them, then unpacked into `target/staged-plugins/<id>/`),
and `make plugin-fmt` / `make plugin-fmt-check` for the plugin crates' formatting.
