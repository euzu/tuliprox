# Sports Enricher

The example plugin of plan section 13 A. It uses most of the plugin system in one place: eight hooks, a network
capability with a bearer-token API, the KV store with TTL keys, the host image cache, a scheduled task, an rpc export that is
also an MCP tool, three sandboxed UI frames and a localized UI (en, de, fr).

| Feature | Mechanism | Code |
| --- | --- | --- |
| Regroup sport channels (`⚽ Football`, `🏀 Basketball`, ...) | `playlist.after-filter`, `scope.groups`, returns `modified` only for changed channels | `src/playlist.rs`, `src/sports.rs` |
| Order channels by league, then match time | `playlist.after-sort`, stable for ties | `src/playlist.rs` |
| Fixtures from an external API | `wasi:http` through the SDK (`http`), bearer = the `api_key` secret, cached under `fx:<league>` with a 1 h TTL | `src/fixtures.rs` |
| Team and league logos | `images::cache` into `channel.logo` | `src/playlist.rs` |
| Synthetic `Home vs Away` programmes | `epg.enrich-programmes` | `src/guide.rs` |
| Richer VOD metadata for sports documentaries | `playlist.enrich-metadata` (writes `details.genre`, `plot`, `description`) | `src/metadata.rs` |
| Remember the last refresh | `events.on-event` (`playlist.update`) | `src/activity.rs` |
| Dashboard metrics | `metrics.collect` | `src/activity.rs` |
| Refresh the fixture cache every 15 minutes | `tasks.run`, task `refresh-fixtures` | `src/api.rs` |
| Fixtures for the UI and MCP | `rpc.call`, export `get-fixtures` | `src/api.rs` |
| Sports Guide page, source-editor tile, EPG overlay | `ui/*.html` frames over the bridge | `ui/` |

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `api_key` | none (write-only secret) | Bearer token of the sports API. Without it the plugin only regroups by name. |
| `api_base_url` | `http://127.0.0.1:18970` | The API, without a trailing slash. |
| `leagues` | `epl`, `laliga`, `nba` | League ids to track, in display order. |
| `group_emoji` | `true` | Prefix the sport groups with an emoji. |

## How the hooks behave

- **Pure and deterministic.** A hook is a function of its input, the settings, the KV store and `clock::now_unix()`.
  No randomness, no wall clock, `BTreeMap` for everything that is ordered into an output.
- **Fail open.** An unreachable or failing API, a missing key or a KV error leaves the data as it was. A failed cold fetch is
  remembered for two minutes (`fx-fail:<league>`) so every hook does not retry it.
- **Hooks read the KV store.** They fetch (one call per league, 5 s timeout) only when a league is cold. `enrich-metadata`,
  `metrics.collect` and the events never call the API.
- **`scope.groups`** is matched by the host against the group a channel has when the hook runs, so the pattern names the sport
  groups the plugin creates as well as `sport`; otherwise `after-sort` would receive nothing.
- **`after-sort`** orders channels inside their group. The host cannot move a channel to another group from `after-sort`,
  which is why the regrouping is done in `after-filter`.
- **`enrich-programmes`** is called per guide channel: only a playlist channel whose EPG id is a `<channel>` of the XMLTV
  guide is offered, with or without programmes.
- **No server-to-frame push.** The frames poll `get-fixtures` every 10 s and show the "Fixtures refreshed" toast when
  `refreshed_at` or `last_update.at` moves.

## Build and package

```bash
rustup target add wasm32-wasip2
cargo build --release --locked --target wasm32-wasip2 \
  --manifest-path plugins/examples/sports-enricher/Cargo.toml --target-dir target/plugin-examples/build
bin/package-plugin.sh plugins/examples/sports-enricher        # target/plugin-examples/sports-enricher.tlpx
```

Install it with `POST /api/v1/plugins` (multipart field `file`), then approve and enable it, or unzip the package into
`<config dir>/plugins/sports-enricher/` and list it under `plugins.plugins` in `config.yml`.

## Mock and demo

```bash
python3 -I plugins/examples/sports-enricher/mock/sports_api.py --port 18970    # fixtures, logos, /_stats
python3 -I plugins/examples/sports-enricher/mock/xtream_provider.py --port 18973   # VOD for enrich-metadata
```

`sports_api.py` accepts `--token`, `--base` (the unix time the fixture offsets count from), and `--down` / `--fail` to simulate
an unreachable or broken API. `GET /_stats` counts the calls. `demo/` holds the playlist, the guide, the config fragments
(`config.yml` with `plugins:`, a `plugin_task` schedule and `mcp:`; `source.yml`; `api-proxy.yml`) and `CHECKS.md`, the steps and
the expected result for every feature.

## Tests

`cargo check --all-targets --manifest-path plugins/examples/sports-enricher/Cargo.toml` compiles the unit tests, which run
on the SDK mock host (`tuliprox_plugin_sdk::mock`). They are written for `cargo test` but are not run by the lane that
produced this example.
