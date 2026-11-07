# HF EPG Matcher: manual checks

Written for the orchestrator's real run in SERVER mode. Nothing here was run when it was written (the example lane
may not run a server), so the expected values below are derived from the code and the mock, not observed.

Ports: tuliprox 18980, HF API mock 18971.

## What the demo is built around

`match-channel` only helps channels the built-in matcher left unresolved. The host asks while it reads the guide's
channel list, offering the guide channels whose own names share a matcher bucket with the playlist name (so the W5
availability gap in `plugin-system-history.md` no longer applies). The demo has:

| Playlist channel | Built-in matcher (smart match on, fuzzy OFF) | Plugin |
| --- | --- | --- |
| `BBC One` | exact name, `bbc.one` | not asked |
| `BBC Two` | exact name, `bbc.two`; this is what keeps `bbc.two` in the guide | not asked |
| `BBC Too` | unresolved (no exact name) | shortlist `bbc.one`, `bbc.two` (same `bbc` prefix bucket); picks `bbc.two` (0.67 against 0.44) |
| `Channel Four` | exact name, `channel.four` | not asked |
| `Channel Fore` | unresolved | shortlist `channel.four`; picks it (0.71) |
| `Mystery Box` | unresolved | shares no bucket with any resolved channel, so it gets no request and stays without an epg id |

`sky.news` is in the guide but referenced by nobody, so it never appears on a shortlist.

## Setup

```sh
REPO=<repo root>                       # the tuliprox checkout
RUN=/tmp/hf-epg-demo                   # scratch run directory
rm -rf "$RUN" && mkdir -p "$RUN/config/plugins/hf-epg-matcher" "$RUN/data"

# 1. build and package the plugin, unpack it where the registry looks
cd "$REPO" && bin/package-plugin.sh plugins/examples/hf-epg-matcher
unzip -o target/plugin-examples/hf-epg-matcher.tlpx -d "$RUN/config/plugins/hf-epg-matcher"

# 2. config: the source needs the absolute demo path
DEMO="$REPO/examples/plugins/hf-epg-matcher/demo"
cp "$DEMO/config.yml" "$DEMO/api-proxy.yml" "$RUN/config/"
sed "s#@DEMO_DIR@#$DEMO#g" "$DEMO/source.yml" > "$RUN/config/source.yml"

# 3. the HF API mock
python3 -I "$REPO/examples/plugins/hf-epg-matcher/mock/hf_api.py" --port 18971 &
curl -s http://127.0.0.1:18971/_stats          # {"calls": 0, "sentences": 0}

# 4. tuliprox (server mode, refresh on boot is in config.yml)
tuliprox -s -p "$RUN/config" -H "$RUN"
```

The plugin is enabled by its `plugins.plugins[]` entry the first time the registry sees it, so no approval call is
needed. Before reading any result, check that it loaded: `curl -s http://127.0.0.1:18980/api/v1/plugins` must list
`hf-epg-matcher` as enabled, and the server log must NOT contain `is invalid`.

To keep the token out of `config.yml`, write `hf_token: ${env:HF_TOKEN}` and start the mock with `--token "$HF_TOKEN"`
so it checks the exact value (without `--token` it accepts any non-empty bearer).

## Check 1: first refresh, the misnamed channels get their guide id

Wait until the server log goes quiet after `update_on_boot`. Then any of:

1. Xtream live list:
   `curl -s 'http://127.0.0.1:18980/player_api.php?username=demo&password=demo&action=get_live_streams'`
   Expected `epg_channel_id`: `BBC One` = `bbc.one`, `BBC Two` = `bbc.two`, **`BBC Too` = `bbc.two`**,
   `Channel Four` = `channel.four`, **`Channel Fore` = `channel.four`**, `Mystery Box` = empty or absent.
2. M3U: `curl -s 'http://127.0.0.1:18980/get.php?username=demo&password=demo&type=m3u'`
   the `BBC Too` line carries `tvg-id="bbc.two"`, the `Channel Fore` line `tvg-id="channel.four"`.
3. Served XMLTV: `curl -s 'http://127.0.0.1:18980/xmltv.php?username=demo&password=demo'`
   contains `<channel id="bbc.two">` and `<channel id="channel.four">` with their programmes, and no `sky.news`.
4. The plugin's own log:
   `curl -s http://127.0.0.1:18980/api/v1/plugins/hf-epg-matcher/logs`
   contains `match-channels: 2 requests, 0 cache hits, 2 model calls, 2 matched, 0 undecided`.
   `curl -s http://127.0.0.1:18980/api/v1/plugins/hf-epg-matcher/stats` shows the `epg.match-channel` hook with
   `modified: 2`.

Mock count after run 1: `curl -s http://127.0.0.1:18971/_stats` gives `"calls": 2` (one model call per unknown name;
the `sentences` figure depends on how many names the host puts on each candidate, so it is not asserted). Call this N.

## Check 2: second refresh, the decisions come from the KV

```sh
curl -s -X POST http://127.0.0.1:18980/api/v1/playlist/update \
  -H 'content-type: application/json' -d '["hf-demo"]'
```

Wait for the log to go quiet. Expected:

- the same `epg_channel_id` values as in Check 1 (`BBC Too` and `Channel Fore` are still matched);
- `curl -s http://127.0.0.1:18971/_stats` still says `"calls": N` (unchanged);
- the newest plugin log line reads `match-channels: 2 requests, 2 cache hits, 0 model calls, 2 matched, 0 undecided`.

(If the host does not ask the plugin at all on the second refresh because it already knows the id, the call count is
unchanged too, but then the log line above is absent; the plugin was not called, which is also a pass for "model called
once per unknown name". Say which of the two happened.)

## Check 3: determinism (orchestrator's frozen-clock double run)

The plugin reads no clock. Run the whole setup twice from an empty `$RUN` with the same
`TULIPROX_PLUGIN_FROZEN_CLOCK` and diff the three outputs of Check 1 (`get_live_streams`, the M3U, the XMLTV): they
must be byte-identical.

## Check 4: the API is down, the playlist still builds

Stop the mock and start it with `--down` (or just stop it), wipe the KV (`rm -rf "$RUN/data/plugins"` or delete the
plugin with `?purge_kv=true`), and refresh again.

- the refresh succeeds and the playlist is complete;
- `BBC Too` and `Channel Fore` have no epg id (`Mystery Box` is unchanged);
- the plugin log has `match-channels: the model API failed (...)` once (the first failure ends the API use of that
  call), then `0 matched, 2 undecided`;
- the KV holds no entry for them: bring the mock back (without `--down`), refresh, and Check 1's values appear with
  fresh model calls (`/_stats` after a `/_reset` shows 2).

## Check 5: settings UI (schema only)

`curl -s http://127.0.0.1:18980/api/v1/plugins/hf-epg-matcher/settings` returns the five settings, with `hf_token`
masked as `********`. The plugin has no `ui/` directory: the Plugins page renders its form from `config-schema.json`.
A `threshold` of 0.9 in `PUT .../settings` followed by a refresh turns `BBC Too` back to unmatched without a new model
call (the best score is what is cached; the threshold is applied on read).
