# YouTube Input Provider

`youtube-input` is an input provider plugin that ingests YouTube channels, playlists, and live streams as native
tuliprox playlist channels. It runs fully sandboxed inside WebAssembly (`wasm32-wasip2`), resolving playable streams
via Invidious REST endpoints with sticky instance failover, accurate live status detection, and KV-cache storage.

## Capabilities

* **`network`**: Fetches YouTube Atom/RSS feeds and queries Invidious REST endpoints.
* **`storage`**: Caches resolved stream URLs in the plugin's KV store using video-scoped keys
  (`yt:str:<video_id>`) with active instance verification and configurable TTL.
* **`process`**: Runs a local `yt-dlp` executable to resolve stream URLs when `resolver` is set to `yt-dlp`.

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

The Web UI source editor builds the same entry: drag the **YouTube** tile from the **Inputs** group of the sidebar
onto the canvas, open the block, and enter the target in `Url`. The block already names `youtube-input`. The tile is
declared with `ui.input_tile` (see [Source editor tiles](./settings.md#source-editor-tiles)).

## Where the configuration lives

Everything a fetch needs belongs to the input itself. The plugin has no global settings: its
`ui.input_form` puts these fields into the input's **Plugin** panel, stored as `plugin_config` in
`source.yml`.

| Field | Meaning |
| --- | --- |
| `channels` | Targets for this input: `@handle`, channel id `UC…`, playlist id `PL…`, or a video. Added to the input's `Url`; a target named twice is fetched once |
| `resolver` | Stream resolver: `invidious` (default) or `yt-dlp` |
| `ytdlp_path` | Optional custom path to `yt-dlp`. If empty, searches `/app/bin/yt-dlp` then `PATH` |
| `invidious_instances` | Mirrors this input resolves streams through, in failover order. Required with `invidious`; optional with `yt-dlp` |
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
      resolver: invidious
      invidious_instances:
        - "http://invidious:3000"
      mode: recent_uploads
      max_uploads: 10

  - name: youtube-live
    type: plugin
    plugin: youtube-input
    url: "https://www.youtube.com/@tagesschau"
    plugin_config:
      resolver: yt-dlp
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

## Invidious instance

The plugin does not talk to YouTube's player directly: it asks an Invidious instance for each video's stream. A YouTube
URL alone is not enough; `invidious_instances` needs at least one instance that serves the video API.

**Public instances usually do not.** YouTube throttles and blocks instances that fetch for many users from a few
addresses, and scrapers abuse public APIs, so most public instances disable `/api/v1/videos` or answer it with `403` or
`access_denied` while their web page and status page still work. The input form's **Check** button asks every instance
for a video and reports `refuses the video API` for such an instance; a fetch whose instances all refuse ends with an
error that says so instead of an empty playlist.

**Run your own instance.** It serves only you, so YouTube does not treat it as a bulk fetcher, and the Invidious team
keeps it working when YouTube changes. It runs as Docker containers, independent of the platform Tuliprox runs on:

1. Follow the Docker Compose installation of the official guide (<https://docs.invidious.io/installation/>). It sets up
   Invidious, its companion service (which talks to YouTube) and a PostgreSQL database, and generates the two secret
   keys (`hmac_key`, `invidious_companion_key`).
2. Put the Invidious container in the same Docker network as Tuliprox, or publish its port (`3000`) to Tuliprox's host.
3. Enter the instance in the input's `invidious_instances`, for example `http://invidious:3000`, and press **Check**.
   It should report `serves videos`.

Streams are requested with `local=true`, so playback runs through your instance as well. Keep the instance updated:
YouTube changes often, and an outdated instance starts refusing videos.

## yt-dlp stream resolver

When `resolver` is set to `yt-dlp`, the plugin resolves streams with a user-provided `yt-dlp` executable instead of
querying an Invidious instance. It resolves uploads to progressive MP4 URLs (`best[ext=mp4][protocol^=http]/best`) and
live broadcasts to HLS manifest URLs.

* **Docker:** Mount the `yt-dlp` binary into the container at `/app/bin/yt-dlp`.
  **Only the `alpine` image works**, and it requires the **musl build** of `yt-dlp`. The minimal `scratch` image
  contains no libc, so an external binary cannot run there. Download the musl build, make it executable and mount
  it:

  ```bash
  curl -L -o ./yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_musllinux
  chmod +x ./yt-dlp
  ```

  ```yaml
  services:
    tuliprox:
      image: ghcr.io/euzu/tuliprox-alpine:latest
      volumes:
        - ./yt-dlp:/app/bin/yt-dlp:ro
  ```

  With `docker run`, use `-v "$PWD/yt-dlp:/app/bin/yt-dlp:ro"`. To update `yt-dlp`, download the file again and
  restart the container.
* **Bare metal:** Place `yt-dlp` at `/app/bin/yt-dlp`, put it on the host's `PATH`, or specify its location in the
  input's `ytdlp_path` field.
* **JavaScript runtime:** YouTube frequently updates player protections. `yt-dlp` may need a JavaScript runtime
  (such as [Deno](https://deno.land/)) installed and available on `PATH` to resolve YouTube streams.
* **Stream URLs and proxy mode:** Stream URLs produced by `yt-dlp` are signed by Google, expire after several hours,
  and are bound to the server's IP address. Client players cannot play these URLs directly; playback must pass through
  Tuliprox (proxied stream mode), not in redirect mode.
* **YouTube bot check:** YouTube may answer a server with "Sign in to confirm you're not a bot". yt-dlp then fails for
  that video and the update card shows this message as the reason. Fewer and less frequent updates make it less likely.
* **Separate audio and video:** YouTube often offers no combined format. The plugin then uses the HLS master playlist
  yt-dlp reports, which carries both.
* **Batch resolution:** All cache-missing videos in an input fetch are resolved in a single batch process run, bounded
  by the remaining fetch budget. Resolved stream URLs are kept in the KV store with their TTL capped below the signed
  URL's expiry.

## Field Reference

Values an input leaves empty keep the plugin's built-in default.

| Field | Default | Description |
| --- | --- | --- |
| `channels` | `[]` | Targets for this input (@handle, channel ID `UC...`, playlist ID `PL...`, or a video). Added to the input's `Url`; one entry may hold a comma-separated list. |
| `resolver` | `"invidious"` | `invidious` (Invidious REST API) or `yt-dlp` (local `yt-dlp` executable). |
| `ytdlp_path` | *(none)* | Optional custom path to `yt-dlp`. If empty, searches `/app/bin/yt-dlp` then `PATH`. |
| `invidious_instances` | *(none)* | Invidious mirrors for stream resolution with sticky failover. Required when `resolver` is `invidious`. |
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
asks for the `network`, `storage`, and `process` capabilities, so the approval has to name all three:

> [!NOTE]
> Approving the plugin now grants the `process` capability, allowing it to start local programs (such as `yt-dlp`) on
> the server as the Tuliprox user. See [Running programs](./security.md#running-programs).

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
  -d "{\"wasmSha256\": \"$HASH\", \"capabilities\": [\"network\", \"storage\", \"process\"], \"mcp\": false}"

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
