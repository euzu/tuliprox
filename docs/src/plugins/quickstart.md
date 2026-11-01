# Quickstart: Template to Running Plugin

This page takes you from this repository to a running plugin that renames one playlist group. You need only this
checkout and this page. Every command is meant to be pasted into one shell session, in order. If you open a new
shell part-way through, run the `export` block below again (and, from step 5 on, the `refresh` helper).

What you build is a plugin from `plugins/examples/template`. Its `playlist.after-filter` hook renames the group named
by the setting `rename_from` to the value of `rename_to`. This quickstart sets `News` to `Plugin News`. The plugin has
no capabilities, so it cannot reach the network or storage.

## Prerequisites

- A Rust toolchain with cargo, and network access to crates.io (the plugin crate and the server both download
  dependencies).
- The WebAssembly target the plugin is built for:

  ```sh
  rustup target add wasm32-wasip2
  ```

- `python3` 3.8 or later (`bin/package-plugin.sh` uses it) and `curl`.
- A shell with bash semantics (bash or zsh).

Set the repository path and the working locations once. Replace the first path:

```sh
export REPO=/absolute/path/to/tuliprox
export QUICKSTART="$HOME/tuliprox-quickstart"   # everything this page creates lives here
export PLUGIN_DIR="$QUICKSTART/my-plugin"
export TULIPROX_HOME="$QUICKSTART/home"
export LOG="$TULIPROX_HOME/server.log"
mkdir -p "$QUICKSTART"
cd "$REPO"
```

## 1. Build tuliprox

```sh
cargo build --release -p tuliprox
```

The binary is `target/release/tuliprox`. The `plugins` cargo feature is on by default (`backend/app/Cargo.toml`), so
this build can load plugins. A first build takes several minutes.

Hint for macOS: if the link step fails with the default linker, set `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=cc` for
the command. Do not set it unless the default fails.

## 2. Render the template

`bin/new-plugin.sh <id> <dest-dir>` copies `plugins/examples/template` to `<dest-dir>` and fills in the plugin id and
the absolute path of this checkout's `plugins/plugin-sdk/`. The id must match `^[a-z][a-z0-9-]{2,40}$`. The destination must
not exist, or must be empty.

```sh
bin/new-plugin.sh my-plugin "$PLUGIN_DIR"
```

The script prints the rendered path and the next command. The rendered crate is outside this repository, and its
`Cargo.toml` depends on the SDK in place.

### The rename rule

The rule is in `plugins/examples/template/src/lib.rs`, so also in `$PLUGIN_DIR/src/lib.rs`. No edit is needed for
this quickstart, because the group names are settings. This is the code that does the work:

```rust
// plugins/examples/template/src/lib.rs
Ok(channels
    .into_iter()
    .filter(|channel| channel.group == settings.rename_from)
    .map(|mut channel| {
        channel.group.clone_from(&settings.rename_to);
        ChannelChange::Modified(channel)
    })
    .collect())
```

It returns only the channels that changed, as `Modified` entries. To change the rule itself, edit
`$PLUGIN_DIR/src/lib.rs` before step 3; for example, compare `channel.group` with a string literal instead of
`settings.rename_from`.

## 3. Build and package

```sh
bin/package-plugin.sh "$PLUGIN_DIR"
```

The script builds `plugin.wasm` with `cargo build --release --target wasm32-wasip2` and zips `manifest.yml`,
`plugin.wasm`, `config-schema.json` and `ui/**` into `target/plugin-examples/my-plugin.tlpx` under the repository. It
prints that path and exits non-zero if the build fails, so run it in the foreground and read its last line.
The build output goes to `target/plugin-examples/build`, not into the plugin directory. See
[Packaging](./packaging.md) for the layout and the upload limits.

## 4. Create the home directory and the config

The server reads `config.yml`, `source.yml` and `api-proxy.yml` from one config directory. These heredocs expand the
shell variables from the first step, so the paths in the files are absolute. `config/plugins` is created empty: the
server watches it for installed plugins (without it the log warns that plugins will not hot reload). The server
rewrites its config files in a normalized form when it starts and when settings are saved, so don't be surprised
when `api-proxy.yml` or `config.yml` look different afterwards.

```sh
mkdir -p "$TULIPROX_HOME/config/plugins" "$TULIPROX_HOME/web"

cat > "$TULIPROX_HOME/config/config.yml" <<EOF
api:
  host: 127.0.0.1
  port: 8901
  web_root: $TULIPROX_HOME/web
log:
  log_level: info
web_ui:
  enabled: true
plugins:
  enabled: true
  directory: plugins
EOF

cat > "$TULIPROX_HOME/config/source.yml" <<EOF
inputs:
  - name: quick-in
    type: m3u
    url: file://$TULIPROX_HOME/playlist.m3u
sources:
  - inputs:
      - quick-in
    targets:
      - name: quick
        output:
          - type: m3u
            filename: quick.m3u
EOF

cat > "$TULIPROX_HOME/config/api-proxy.yml" <<EOF
server:
  - name: default
    protocol: http
    host: 127.0.0.1
    port: "8901"
    timezone: UTC
    message: plugin quickstart
user:
  - target: quick
    credentials:
      - username: demo
        password: demo
        proxy: reverse
        server: default
        status: Active
EOF

cat > "$TULIPROX_HOME/playlist.m3u" <<EOF
#EXTM3U
#EXTINF:-1 tvg-id="news.one" group-title="News",News One
http://127.0.0.1:9/news-one.ts
#EXTINF:-1 tvg-id="sport.one" group-title="Sport",Sport One
http://127.0.0.1:9/sport-one.ts
EOF
```

About these files:

- The input and the user follow `plugins/examples/hf-epg-matcher/demo/source.yml` and `api-proxy.yml`. The `message`
  line of the server entry is required: without it the server fails with ``missing field `message` ``.
- `web_ui: {enabled: true}` with no `auth` block means the web API has no authentication, so no token is needed in
  this quickstart. Do not add `auth: {enabled: false}` alone: the server then fails with `missing field issuer`.
- `web_ui.enabled` is what mounts the `/api/v1` routes that this page calls. `web_root` only has to be an existing
  directory. An empty one is enough, because this page does not open the web UI in a browser.
- Do not write both `working_dir` and `storage_dir`. They are one field under two names, and writing both fails with
  `duplicate field storage_dir`. This config writes neither, so the server stores its data under the home directory.
- `plugins: {enabled: true, directory: plugins}` has no `plugins[]` list. `directory` is relative to the config
  directory. The settings call in step 8 writes the `plugins[]` entry into `config.yml`.

## 5. Start the server and take a baseline

Start the server in the background with its log in a file:

```sh
nohup "$REPO/target/release/tuliprox" -s -p "$TULIPROX_HOME/config" -H "$TULIPROX_HOME" > "$LOG" 2>&1 &
echo $! > "$TULIPROX_HOME/server.pid"
for i in $(seq 60); do curl -s -o /dev/null http://127.0.0.1:8901/api/v1/status && break; sleep 1; done
```

`-s` is server mode, `-p` is the config directory and `-H` is the home directory. `nohup` keeps the server running
if the shell that started it ends. The loop returns when the server answers, or after 60 s; if the server never
answers, read `$LOG`.

Define a helper that starts a refresh of the target `quick` and waits until the server logs that it is finished:

```sh
refresh() {
  n=$(grep -c 'processor::playlist\] stats:' "$LOG")
  curl -s -X POST http://127.0.0.1:8901/api/v1/playlist/update \
    -H 'Content-Type: application/json' -d '["quick"]'
  echo
  for i in $(seq 120); do
    [ "$(grep -c 'processor::playlist\] stats:' "$LOG")" -gt "$n" ] && return 0
    sleep 1
  done
  echo "refresh did not finish within 120 s; read $LOG" >&2; return 1
}
```

The route answers `202` with `{"run_id": ...}` at once. The refresh is done when the log gets a new
`tuliprox_processing::processor::playlist] stats:` line. The body is a list of target names. The object form
`{"targets": ["quick"]}` is also accepted.

Refresh once and read the playlist before the plugin exists:

```sh
refresh
curl -s "http://127.0.0.1:8901/get.php?username=demo&password=demo&type=m3u_plus"
```

The two entries carry `group-title="News"` and `group-title="Sport"`. `/get.php` with a user from `api-proxy.yml` is
how the target's M3U is served.

## 6. Upload the package

The upload is a multipart form with the file in the field `file`. Nothing is installed yet. The server unpacks the
package, checks it and answers with a review:

```sh
curl -s -F "file=@$REPO/target/plugin-examples/my-plugin.tlpx" \
  http://127.0.0.1:8901/api/v1/plugins | tee "$TULIPROX_HOME/review.json"
echo
```

The answer is `200` with a camelCase review: `id`, `version`, `wasmSha256`, `capabilities`, `hooks`, `secrets`,
`customUi` and `mcpRequested`, among others. For this plugin `capabilities` is `[]`, `hooks` is
`["playlist.after-filter"]` and `secrets` is `["api_token"]`: the template declares one write-only secret to show
how secrets work, but never reads it, so you can leave it unset. A `422` answer lists every problem in `problems`. The route is in
`backend/app/src/api/endpoints/plugins_install_api.rs`.

Keep the hash from the review for the next step:

```sh
HASH=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["wasmSha256"])' "$TULIPROX_HOME/review.json")
echo "$HASH"
```

## 7. Approve

Approval says: I reviewed this exact package. The body repeats the reviewed hash and the reviewed capability names.
The keys are camelCase, and `wasm_sha256` is refused with `409 wasm hash mismatch`. This plugin asks for no
capabilities and no MCP tools:

```sh
curl -s -X PUT http://127.0.0.1:8901/api/v1/plugins/my-plugin/approve \
  -H 'Content-Type: application/json' \
  -d "{\"wasmSha256\": \"$HASH\", \"capabilities\": [], \"mcp\": false}"
echo
```

The answer is `200` with the plugin summary, in `state` `loaded` with `enabled` false. Approval does not enable the
plugin. The server records the approval in `$TULIPROX_HOME/data/plugins/approvals.json`. A plugin that does ask for
capabilities needs their names listed in `capabilities`; see [Security](./security.md).

## 8. Set the settings

The body wraps the values in a `settings` object. They are checked against `config-schema.json`:

```sh
curl -s -X PUT http://127.0.0.1:8901/api/v1/plugins/my-plugin/settings \
  -H 'Content-Type: application/json' \
  -d '{"settings": {"rename_from": "News", "rename_to": "Plugin News"}}'
echo
```

The answer is `200` with the masked settings, the schema, the list of secret keys and any `warnings`. A value that
breaks the schema is refused with `422`. The server rewrites `config.yml` with a `plugins.plugins[]` entry that holds
the values, and keeps the previous file in the backup directory (`$TULIPROX_HOME/data/backup` here). The rewrite
comes from the parsed config, so comments in `config.yml` are not kept.

## 9. Enable

```sh
curl -s -X PUT http://127.0.0.1:8901/api/v1/plugins/my-plugin/enable
echo
```

The answer is `200` with the summary, and `state` is `enabled`. Enabling a plugin that is not approved, or is invalid,
is a `409`.

## 10. Refresh and read the playlist

```sh
refresh
curl -s "http://127.0.0.1:8901/get.php?username=demo&password=demo&type=m3u_plus"
```

The `News One` entry now carries `group-title="Plugin News"`, and `Sport One` keeps `group-title="Sport"`. That is the
result of the quickstart: the plugin changed the playlist.

## How to tell it worked

- The playlist from step 10 has `Plugin News` where step 5 had `News`.
- `curl -s http://127.0.0.1:8901/api/v1/plugins` lists `my-plugin` with `"state": "enabled"`.
- `curl -s http://127.0.0.1:8901/api/v1/plugins/my-plugin/stats` shows `"calls":1` and, under `hooks`, the entry
  `{"hook":"after-filter","kept":2,"dropped":0,"modified":1,...}`.

## If it did not work

An invalid plugin fails open: tuliprox keeps running and the playlist comes out unchanged. So the first check is the
log:

```sh
grep 'is invalid' "$LOG"
```

The line reads ``plugin `my-plugin` is invalid: <reason>``. Other things to check:

- Upload `422`: read `problems` in the body.
- Approve `409`: the body does not match the package you reviewed. Upload again and approve the new hash.
- Enable `409`: the plugin is not approved yet, or is invalid. The message says which.
- Any plugin route answers `404`: plugins are off in `config.yml`, or the server was built without the `plugins`
  feature.
- The group did not change: the group name in your playlist must equal `rename_from` exactly, and `rename_from` must
  differ from `rename_to`.
- The server is not running: `tail "$LOG"`. A config error is printed there and the process exits.

## Stop the server

```sh
kill "$(cat "$TULIPROX_HOME/server.pid")"
```

Everything this page created is under `$QUICKSTART`, plus the package and build output in `$REPO/target`. To remove
it: `rm -rf "$QUICKSTART"`.

## If you turn auth on

This config has no `web_ui.auth` block, so the API is open and no token is needed. With auth on, every call above
needs an `Authorization: Bearer <token>` header. How to configure auth is covered in
[config.yml](../configuration/config.md). This page did not run that case.

<!-- UNVERIFIED: the auth-on case (token route and the plugin management permission, SystemWrite) was not run. -->

## Next steps

- Read the generated `$PLUGIN_DIR/README.md` and [Packaging](./packaging.md).
- Add a capability or a secret: [Settings](./settings.md) and [Security](./security.md).
- Add a page to the web UI: [Custom UI](./custom-ui.md). The template already ships a menu page in `ui/`.
