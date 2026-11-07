# Sports Enricher: manual checks

These checks run the plugin against a real tuliprox in server mode. They were written from the code and
the demo data, not from a run: every "expect" below is a prediction. A different result is a finding, not
a typo in the step.

Ports: tuliprox **18980**, sports API mock **18970**, Xtream provider mock **18973**.

## 0. Setup

```bash
REPO=/path/to/tuliprox                     # a checkout with this example
CFG=/tmp/sports-demo/config                # the tuliprox config directory of the run
BASE=1793491200                            # 2026-11-01 00:00 UTC, the mock's fixture base
NOW=$((BASE + 36000))                      # 2026-11-01 10:00 UTC, the frozen host clock of the checks
API=http://127.0.0.1:18980/api/v1
EX=$REPO/examples/plugins/sports-enricher

# build and package (wasm32-wasip2 target, see README)
cd "$REPO" && bin/package-plugin.sh plugins/examples/sports-enricher   # -> target/plugin-examples/sports-enricher.tlpx

# the two mocks
python3 -I $EX/mock/sports_api.py --port 18970 --base $BASE &
python3 -I $EX/mock/xtream_provider.py --port 18973 &

# config: merge demo/config.yml, demo/source.yml and demo/api-proxy.yml into $CFG (api.port 18980).
# Leave $CFG/plugins empty: the plugin is installed through the API in section 1.
mkdir -p "$CFG/plugins"
```

Start tuliprox in server mode with the frozen host clock (the variable only exists in this repo's `plugin_state.rs`):

```bash
TULIPROX_PLUGIN_FROZEN_CLOCK=$NOW <tuliprox binary> -s -p "$CFG"      # your usual config-dir flag
```

The UI checks (section 4) need a moving clock for the refresh toast, so they run in a separate boot without the variable
and with the mock started without `--base`.

Helper used below (prints one JSON field):

```bash
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1].split("."): d = d[int(k)] if k.isdigit() else d[k]
print(d if not isinstance(d,(dict,list)) else json.dumps(d))' "$1"; }
```

## 1. Install, review, approve, enable (capabilities, `invalid` trap)

1. `curl -s -F file=@$REPO/target/plugin-examples/sports-enricher.tlpx $API/plugins` returns the review.
   Expect `id: sports-enricher`, `capabilities` = network, storage, epg_write, image_cache, `hooks` = the eight manifest hooks,
   `events: ["playlist.update"]`, `tasks: ["refresh-fixtures"]`, `secrets: ["api_key"]`, `customUi: true`, `mcpRequested: true`.
2. `SHA=$(...)` the `wasmSha256` of that answer, then
   `curl -s -X PUT $API/plugins/sports-enricher/approve -H 'content-type: application/json' -d "{\"wasmSha256\":\"$SHA\",\"capabilities\":[\"epg_write\",\"image_cache\",\"network\",\"storage\"],\"mcp\":true}"`.
   Expect state `approved` (or `loaded`).
3. `curl -s -X PUT $API/plugins/sports-enricher/enable`, then `curl -s $API/plugins | grep sports-enricher`. Expect state `enabled`.
4. Grep the server log for `is invalid`: expect no line. An invalid plugin fails open silently, and every later check would
   then show the baseline playlist. The component must export lifecycle plus exactly the eight hook interfaces of the manifest.

## 2. Settings (`config-schema.json` and the secret)

1. `curl -s $API/plugins/sports-enricher/settings`. Expect `api_key` as `********`, `api_base_url`, `leagues`, `group_emoji: true`.
2. Config page, plugin section (web UI at <http://127.0.0.1:18980/>): the SchemaForm shows four fields; the API key is a password
   field with no default and the leagues are a list.

## 3. Playlist, EPG, VOD and metrics (server outputs)

Trigger boot 1: `curl -s -X POST $API/playlist/update -H 'content-type: application/json' -d '["sports"]'`
(or restart with `update_on_boot: true`). Wait until the server log goes quiet.

Outputs are read as the demo user:

```bash
U='username=demo&password=demo'
S=http://127.0.0.1:18980
curl -s "$S/get.php?$U&type=m3u_plus&output=ts"            # the M3U output
curl -s "$S/player_api.php?$U&action=get_live_categories"
curl -s "$S/xmltv.php?$U"
```

### 3.1 Regroup sport channels (`after-filter`, scope)

Expect in the M3U `group-title` lines: `⚽ Football` (Sky Sports Premier League, TNT Sports 1, LaLiga TV HD, Planet Football),
`🏀 Basketball` (ESPN NBA, NBA TV), `🏈 American Football` (NFL Network), `🎾 Tennis` (Eurosport Tennis and the Xtream
Eurosport Tennis HD). `Arena Highlights` stays in `UK Sports` (no sport keyword, no fixture), `BBC One` and `CNN International`
stay in `News`, `Movie Max` in `Movies`, and the groups `Sports`, `Sport HD` and `Provider Sports` are gone.

- `TNT Sports 1` has no sport in its name: it is regrouped because it broadcasts a cached fixture (EPG id `tnt.sports.1.uk`).
- The non-sport groups never reach the plugin (scope): `GET $API/plugins/sports-enricher/stats` shows `after-filter`
  with `kept` + `dropped` around 10 (the scoped channels), not 16. Expect `modified` about 9.
- Group emoji toggle: `curl -s -X PUT $API/plugins/sports-enricher/settings -H 'content-type: application/json' -d '{"settings":{"group_emoji":false}}'`
  (re-send `api_key` if the route needs the whole object), refresh, expect `Football`, `Basketball` without emoji. Set it back to `true`.

### 3.2 Order by league, then match time (`after-sort`)

In the `⚽ Football` group (`get_live_streams` with that `category_id`, or the M3U order) expect exactly:
Sky Sports Premier League (epl, 20:45 on day 0), TNT Sports 1 (epl, day 1 16:30), LaLiga TV HD (laliga, rank after epl),
Planet Football (no league, last). The M3U lists them in the opposite order (`demo/playlist.m3u`), so the order is the plugin's.
In `🏀 Basketball`: ESPN NBA (has a fixture) before NBA TV (league named in its name only).
Repeat with the clock frozen twice (restart): the order is identical (stable for ties, by handle).

Note on `scope`: the manifest pattern lists the sport names as well as `sport`, because the host matches the scope against the
group a channel has when the hook runs. After `after-filter` the groups are `Football` and the like, which `(?i)sport` alone does
not match, and `after-sort` would never see them.

### 3.3 Fixtures over HTTP, KV cache with TTL, refresh task

- `curl -s http://127.0.0.1:18970/_stats`: `fixtures_calls` is 3 (one per league; 3 to 6 if the two inputs raced on the cold
  cache), `unauthorized` is 0 (the bearer token is the `api_key` secret).
- Run the task: `curl -s -X POST $API/plugins/sports-enricher/tasks/refresh-fixtures/run`. Expect HTTP 200 and
  `refreshed 3 leagues, 10 fixtures`; mock `fixtures_calls` +3. A second request while one runs answers 409 (`concurrency: skip`).
- `curl -s $API/plugins/sports-enricher/tasks` lists `refresh-fixtures` with its default schedule.
- Schedule: `demo/config.yml` has the `type: plugin_task` entry; the task runs every 15 minutes (log line `refreshed 3 leagues`).
- TTL: the cache lives 1 h. With the host clock at `NOW + 3601` s the next hook fetches again (needs a clock you can move; with the
  frozen clock this is a unit test: `fixtures::tests::the_cache_expires_and_no_key_means_no_fetch`, written, not run).
- Hooks fetch only on a cold cache: restart tuliprox (KV persists) and refresh; mock `fixtures_calls` does not move.

### 3.4 Logos (`cache-image`)

In the M3U the channels that had `tvg-logo=""` (Planet Football, LaLiga TV HD, Sky Sports Premier League, NBA TV, NFL Network)
and the Xtream Eurosport Tennis HD now carry a non-empty `tvg-logo`. Mock `/_stats` `logo_requests` is 6 (football generic,
LaLiga, Premier League, NBA, American Football generic, tennis generic) and stays 6 after another refresh: the host image
cache answers a cached URL without downloading. Channels that already had a logo keep it.
A logo resolves in this order: a team named in the channel name, the league of the match the channel broadcasts, a league the
name mentions, the generic logo of the sport.

### 3.5 Synthetic EPG (`enrich-programmes`)

`curl -s "$S/xmltv.php?$U"` (UTC server timezone). Expect `<programme>` entries whose `title` is the match and whose `start` looks like
`20261101204500 +0000`:

| channel id | programmes added by the plugin |
| --- | --- |
| `sky.sports.premierleague.uk` | Liverpool vs Arsenal (2026-11-01 20:45), Tottenham vs Newcastle (11-02 19:00) |
| `tnt.sports.1.uk` | Chelsea vs Manchester City (11-02 16:30), Everton vs Aston Villa (11-03 15:00); the guide's own `Sports Talk Live` stays |
| `laliga.tv.es` | Real Madrid vs Barcelona (11-02 21:00), Sevilla vs Valencia (11-03 19:00), Atletico Madrid vs Real Betis (11-04 20:00) |
| `espn.nba.us` | Lakers vs Celtics (11-02 01:00), Warriors vs Bulls (11-03 02:00), Knicks vs Heat (11-04 00:30) |

Each synthetic programme runs two hours (football) or two and a half (basketball), has `<desc>` like
`Premier League, round 12 at Anfield.` and the categories `Sports` and the sport. `nba.tv.us` and `bbc.one.uk` get nothing.

The host calls `enrich-programmes` only for channels of the guide, and `demo/guide.xml` lists a `<channel>` for every
broadcaster id on purpose; four of them have no `<programme>` at all, which shows the hook fills an empty channel. A playlist
channel with no guide entry would never be offered. That is a limit of the hook, not of the plugin.

### 3.6 VOD metadata (`enrich-metadata`) — needs boot 2

The input VOD store does not exist during the first update, so the titles resolve on the second one. **Restart tuliprox**
(same config, `update_on_boot: true`), wait for the log to go quiet, then:

```bash
curl -s "$S/player_api.php?$U&action=get_vod_streams"        # note the stream_id of "Anfield Stories..." and "Courtside..."
curl -s "$S/player_api.php?$U&action=get_vod_info&vod_id=<stream_id>"
```

Expect, for `Anfield Stories: A Football Documentary`:
`info.genre` = `Documentary, Sports`; `info.plot` = the provider plot, a blank line, then
`[Sports Enricher] ⚽ Football documentary. Next in Premier League: Liverpool vs Arsenal, 2026-11-01 20:45 UTC.`;
`info.description` = the `[Sports Enricher] ...` sentence (the provider sent none). `Courtside: The NBA Documentary` gets the
basketball sentence (`Next in NBA: Lakers vs Celtics, 2026-11-02 01:00 UTC.`). `Pasta Masters` is unchanged.
Mock `xtream_provider.py` `/_stats` shows `get_vod_info` once per title (the record is stored, then not resolved again).

What is visible: only the `details` block of the stored record reaches the served info (`plot`, `description`, `genre`, `cast`,
...). Top-level fields (`rating`, `trailer`, `tmdb`) reach `get_vod_streams`, and `plot` is not one of them. The plugin writes
into `details` for that reason, and skips a record with no `details`.

### 3.7 `on-event(playlist.update)`, metrics, rpc

- `curl -s -X POST $API/plugins/sports-enricher/rpc/get-fixtures -H 'content-type: application/json' -d '{"leagues":["nba"],"limit":2}'`
  Expect `fixtures` = the two earliest NBA matches (`nba-3001`, `nba-3002`), `now` = the frozen clock, `configured: true`, and
  `last_update` with `state: "Success"`, `regrouped` about 9, `sport_channels` about 10, `programmes_added` about 10 and, after
  boot 2, `vod_enriched` 2. The `at` field is the event's timestamp.
- `curl -s $API/plugins/metrics`: `sports-enricher.fixtures_cached` = 10, `sports-enricher.api_calls_today` = the mock's
  `fixtures_calls`, `sports-enricher.channels_enriched` = `regrouped` of the last refresh. Labels are `metric.*` locale keys
  (the dashboard card shows "Fixtures cached" and so on, in the UI language).
- Dashboard (web UI): the plugin metrics cards show the same three values.

### 3.8 Fail open

- Mock `--down` (restart it): `curl -s -X POST $API/plugins/sports-enricher/tasks/refresh-fixtures/run` fails
  (HTTP 5xx, the run's error names the league). With the KV warm the playlist keeps its ordering and logos.
  To test a cold dead API: `curl -s -X DELETE "$API/plugins/sports-enricher?purge_kv=true"`, install again (section 1), refresh. Expect the
  playlist to be produced, channels regrouped by name only (`TNT Sports 1` stays in `UK Sports`, no logos, no ordering, no added
  programmes) and `cold fetch of league ... failed, continuing without it` in `GET $API/plugins/sports-enricher/logs`.
  After the failure no further request reaches the mock for two minutes (the `fx-fail` back-off).
- Mock `--fail` (HTTP 500): same result.
- Wrong key (`api_key: nope`): mock `unauthorized` counts, playlist still produced.

## 4. UI (web UI, isolated browser context)

Boot without `TULIPROX_PLUGIN_FROZEN_CLOCK`, mock without `--base`, then open <http://127.0.0.1:18980/> in a fresh
`browser.newContext()` (the shared Playwright browser is driven by other sessions).

- **Menu `Sports Guide`** (title from `menu.title`; `de` and `fr` in the language switch give `Sport-Guide`, `Guide du sport`):
  a table of the next fixtures, a league select (filters the rows) and a reload button. The status column shows `Live` for a match
  in progress and `Upcoming` otherwise.
- **Source editor tile**: open a source (`sports-m3u`): the tile "Sports Enricher" lists leagues, fixtures cached, API calls today,
  last fixture refresh, last playlist refresh and the enriched counts.
- **EPG overlay**: Playlist, EPG view, pick `Sky Sports Premier League`, focus the programme `Liverpool vs Arsenal`: the overlay
  shows match, league and round, kick-off, status, venue and the stats (home form, away form, head to head). A programme that is
  not a fixture (`Sports Talk Live`) shows "This programme is not one of the tracked fixtures."
- **Toast**: with the Sports Guide open, run the task (`POST .../tasks/refresh-fixtures/run`). Within 10 s the toast
  "Fixtures refreshed" appears. There is no server-to-frame push in the bridge, so a frame learns about a refresh by polling
  `get-fixtures` every 10 s and comparing `refreshed_at` and `last_update.at`: a plan disagreement (the plan says
  `on-event(playlist.update)` plus the bridge `toast`, but an event handler cannot reach a frame).
- **Theme and locale**: toggle the dark theme; the frames follow (`--theme-brand-*` variables). Switch to German: strings change
  without a reload.
- Browser console: no CSP violation (frames load only `../../_sdk/bridge.js`, `common.js` and the page script; no inline
  script, no remote image).

## 5. MCP: the `get-fixtures` tool needs all three switches

Switches: (1) the manifest export has `mcp: true`; (2) the approval says `"mcp": true`; (3) config `mcp.enabled: true` and
`mcp.plugin_tools: true`. The MCP block is mounted at startup, so restart after editing `config.yml`.

```bash
MCP=http://127.0.0.1:18980/mcp
H='content-type: application/json'; A='accept: application/json, text/event-stream'
curl -s -X POST $MCP -H "$H" -H "$A" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | grep -o 'plugin_sports_enricher_get_fixtures'
curl -s -X POST $MCP -H "$H" -H "$A" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"plugin_sports_enricher_get_fixtures","arguments":{"leagues":["nba"],"limit":2}}}'
```

| manifest `mcp` | approval `mcp` | `mcp.plugin_tools` | `tools/list` has `plugin_sports_enricher_get_fixtures` |
| --- | --- | --- | --- |
| true | true | true | yes; `tools/call` returns the two NBA fixtures |
| true | false (approve again without `mcp`) | true | no |
| true | true | false (restart) | no |

`mcp.enabled: false` removes the route altogether. The tool is `read`, so `read_only` does not hide it; a `write` export would never be
offered while the server is read-only.

## 6. Determinism (two runs, frozen clock)

With `TULIPROX_PLUGIN_FROZEN_CLOCK=$NOW`, the mock started with `--base $BASE` and a warm cache (run the task first), do two
full refreshes (restart in between, same KV) and diff the outputs:

```bash
for n in 1 2; do
  curl -s "$S/get.php?$U&type=m3u_plus&output=ts" | sed 's/tvg-logo="[^"]*"/tvg-logo=""/' > /tmp/sports-$n.m3u
  curl -s "$S/xmltv.php?$U" > /tmp/sports-$n.xml
done
diff /tmp/sports-1.m3u /tmp/sports-2.m3u && diff /tmp/sports-1.xml /tmp/sports-2.xml && echo identical
```

Expect `identical`. (Logo URLs are masked only to ignore host-generated resource tokens; compare them separately if they are
stable in your build.) The hooks use no randomness, no wall clock (`clock::now_unix()` only), `BTreeMap` for every map that
reaches an output, and sort with explicit tie-breaks.
