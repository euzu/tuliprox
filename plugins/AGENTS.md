# Plugins: instructions for agents

This folder holds the tuliprox plugin SDK, the examples, and every plugin that lives in this repository and is built
from source. Read this file before you create or change a plugin. The reference documentation is in
`docs/src/plugins/` (start with `overview.md`, `hooks.md`, `settings.md`, `security.md`, `packaging.md`); this file
says what an agent has to do, and in what order, so the result is complete the first time.

## What is where

| Path | What it is |
| --- | --- |
| `plugins/plugin-api/` | The WIT contract (`wit/tuliprox-plugin.wit`, package `tuliprox:plugin@1.0.0`). Frozen: never change it for one plugin |
| `plugins/plugin-sdk/`, `plugin-sdk-macros/` | The Rust SDK a plugin is written against (`tuliprox_plugin_sdk::prelude::*`) |
| `plugins/examples/` | Teaching examples and the template (`template`). Not shipped, not maintained as products |
| `plugins/notifiers/<id>/` | A notification plugin (a message channel) that belongs to this repository and is built from source, for example `plugins/notifiers/discord-notifier` |

Plugins are grouped by kind, `plugins/<group>/<id>`. Today the only group is `notifiers` (every plugin that is a message
channel; ids end in `-notifier`). A plugin of another kind gets its own group folder, named with a plural noun, the day
the first one exists, and not before. Keep the SDK dependency as `../../plugin-sdk` so every plugin sits two levels
below `plugins/`.

A plugin is a WebAssembly component (`wasm32-wasip2`) plus a manifest, an optional settings schema, optional UI and
translations, zipped as a `.tlpx`. Plugin crates are **not** workspace members: each one has its own empty `[workspace]`
table, which is all it takes. Do not list a plugin in the root `Cargo.toml` (neither in `members` nor in `exclude`).

## Rules that are never negotiable

1. **The backend never knows a plugin.** No plugin id, channel name, setting or message format may appear in
   `backend/`, `shared/` or `frontend/`. Everything a plugin does, shows or formats comes from the plugin: its code,
   its schema, its locale files. If the host lacks a capability, add a generic one (for example a field every plugin can
   use) and never a special case. Check with `git grep -i <name> backend shared frontend` before you finish.
2. **Every user-visible text is translatable and ships translated** (see [Translations](#translations)).
3. **Secrets are never logged, echoed or put in an error.** A webhook URL, a token and a password are all secrets.
4. **Ask only for the capabilities the plugin uses.** The admin approves them; a needless `network` or `storage` is a bug.
5. **Never commit build output.** No `.wasm`, no `.tlpx`, no `target/` inside a plugin folder.
6. **Do not edit the WIT or the host to suit one plugin.** Additive data goes into the JSON the host already passes
   (for example the notification `payload`); a new interface is a separate, reviewed change.

## Layout of a plugin that lives in this repository

```text
plugins/notifiers/<id>/
  Cargo.toml          own empty [workspace] table (keep it), crate-type cdylib, path dependency on ../../plugin-sdk
  Cargo.lock          committed (the package script builds with --locked when it exists)
  manifest.yml        identity, capabilities, hooks, resources, ui
  config-schema.json  the settings form (when the plugin has settings)
  src/lib.rs          the #[tuliprox_plugin] module: settings struct and hook impls
  src/<logic>.rs      pure logic, testable without the settings static
  ui/locales/<lang>.json   translations, one file per language
  ui/                 frames and scripts only when the plugin needs custom UI
```

Start from `bin/new-plugin.sh <id> <dest-dir>` (renders `plugins/examples/template`) or copy a finished plugin such as
`plugins/notifiers/discord-notifier`. The id must match `^[a-z][a-z0-9-]{2,40}$`; `metrics`, `staging` and `plugins-prev` are reserved.

Wiring a new in-repository plugin into the repository is part of the task, not an afterthought:

- [ ] `Makefile`: add the id to `NOTIFIER_PLUGINS` (so `make plugin-notifiers` builds it) and a `plugin-<id>` target that runs
      `./bin/package-plugin.sh plugins/notifiers/<id> --out target/plugin-<id>/<id>.tlpx` (copy `plugin-discord-notifier`)
- [ ] `docs/src/plugins/<id>.md` and a link in `docs/src/SUMMARY.md`
- [ ] `CHANGELOG.md`: an entry under `## Unreleased`, in the right section (a removed or renamed config key is a breaking change)
- [ ] `plugins/notifiers/<id>/Cargo.lock` committed

## The manifest

`manifest.yml` is read by a line-based scanner in `bin/package-plugin.sh` and by the host's YAML parser. Keep to the
shapes the examples use (flow lists such as `locales: [en, ru]`, scalar paths) or the script may refuse a manifest the
host would accept. Unknown top-level keys are ignored by the host, so a typo is silent: copy key names exactly.

- `id`, `name`, `version` (semver, the plugin's own), `api_version: 1`, `author`, `description` are all your own values.
- `capabilities:` only what is used: `network`, `storage`, `epg_write`, `image_cache`, `notifications`, `stream_headers`.
- `hooks:` must match the component's exports exactly, in both directions. A mismatch makes the plugin **invalid**, and an
  invalid plugin fails open (it is skipped, the pipeline carries on, and the output looks like the baseline).
- `ui.config_section.schema` points at the settings schema; `ui.locales` lists the languages (see below).
- `resources:` may only lower the host's limits (`max_exec_ms`, `max_memory_mb`, `max_instances`); it can never raise them.

## Settings schema (`config-schema.json`)

- Supported: `type` (`string`, `number`, `integer`, `boolean`, `object`, `array`), `enum`, `minimum`, `maximum`,
  `required`, `properties`, `items`, `additionalProperties`, `title`, `description`, `default`, `writeOnly`, and a
  top-level integer `version`. **Not supported** (the package is refused): `oneOf`, `anyOf`, `allOf`, `$ref`, `if`, `then`,
  `else`, `not`, `dependentSchemas`, `patternProperties`.
- The form renders objects only one level deep, lists of scalars, enums and secrets. Keep the schema flat. The host
  orders form fields by key (alphabetical), so do not rely on the order you wrote.
- A secret has `"writeOnly": true`, **no `default`**, and is read in Rust as `Secret<String>` (`.expose()` for the value).
  Tell users to store it as `${env:VAR}`. A `routing` object (`notify_on`, `min_severity`, `quiet_hours`, `max_per_hour`,
  `dedup_window_secs`) is read by the host for a notification channel; mirror it in the schema so it is editable, but do
  not read it in the plugin.
- Changing the shape of stored settings (a rename, a removed field) means raising the schema `version` and exporting
  `settings.migrate`; otherwise old configs show a "settings need review" notice or stop the plugin.
- In Rust, `#[tuliprox_plugin_settings] pub struct Settings { ... }` with `#[serde(default)]` on fields, so a partly
  filled config still parses. A settings parse failure is permanent, it will not heal on a retry.

## Translations

The host app speaks the languages in `frontend/public/assets/i18n/index.json` (today `en`, `ru`, `ar`, `es`). A plugin
ships **all of them**, so its settings form reads in the language of the UI. Never hard-code English into the schema.

Rules:

1. Every `title` and `description` in `config-schema.json` is a **key**, not prose. Use the pattern
   `settings.<path>.title` and `settings.<path>.description` (`settings.title` for the root,
   `settings.routing.quiet_hours.title` for a nested field).
2. `ui/locales/<lang>.json` is one **flat** JSON object of strings, one file per language. `en` is the fallback language
   and always ships (the host enforces it as soon as a `title_key` is used). A key missing from a language falls back to `en`, and a value that is not a key is shown as written, so a
   forgotten key shows as the raw key text, which is the symptom of a missing translation.
3. The manifest lists the languages: `ui: { locales: [en, ru, es, ar], config_section: { schema: config-schema.json } }`.
4. Every language file has **exactly the same keys** as `en`. Translate for meaning, keep placeholders such as
   `${env:VAR}`, `{name}`, option names (`embed`, `notify_on`), units and code identifiers untouched. RTL languages need
   no special markup.
5. Keys referenced from the manifest (`title_key` of a menu, tab or contribution) must exist in `en.json`; the host
   refuses the package otherwise. Custom UI frames call `t(key, params)` from the bridge for the same files.
6. The host translates only schema `title` and `description`. **Not translated:** enum values (`embed`, `warn`),
   field names, and anything the plugin sends to an outside service (a message body, an embed). Keep user-facing
   enum values self-explanatory and describe them in the field's description.
7. A locale file is at most 1 MiB. Write them as UTF-8 with real characters, not `\u` escapes.

Check before you finish (run from the repository root, with the plugin directory as the argument):

```sh
python3 -I - plugins/notifiers/<id> <<'PY'
import json, sys, pathlib
root = pathlib.Path(sys.argv[1])
schema = json.loads((root / "config-schema.json").read_text())
keys = []
def walk(node):
    if isinstance(node, dict):
        for k, v in node.items():
            if k in ("title", "description") and isinstance(v, str): keys.append(v)
            else: walk(v)
    elif isinstance(node, list):
        for item in node: walk(item)
walk(schema)
langs = sorted(p.stem for p in (root / "ui" / "locales").glob("*.json"))
index = json.loads(pathlib.Path("frontend/public/assets/i18n/index.json").read_text())
wanted = sorted(l["code"] for l in index["languages"])
assert langs == wanted, f"languages {langs} != app languages {wanted}"
en = json.loads((root / "ui/locales/en.json").read_text())
for lang in langs:
    strings = json.loads((root / f"ui/locales/{lang}.json").read_text())
    assert set(strings) == set(en), f"{lang}: keys differ: {set(strings) ^ set(en)}"
    assert all(isinstance(v, str) for v in strings.values()), f"{lang}: non-string value"
missing = sorted(set(keys) - set(en))
assert not missing, f"schema keys without an English string: {missing}"
print("ok:", len(keys), "schema texts,", len(langs), "languages")
PY
```

## Code

```rust
use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin(manifest = "manifest.yml")]
mod plugin {
    use super::*;

    #[tuliprox_plugin_settings]
    pub struct Settings {
        #[serde(default)]
        pub url: Secret<String>,
    }

    pub struct MyPlugin;

    impl NotificationHooks for MyPlugin {
        fn send_notification(msg: Notification) -> Result<(), DeliveryError> {
            let settings = Settings::current().map_err(DeliveryError::Permanent)?;
            crate::logic::deliver(settings.url.expose(), &msg)
        }
    }
}
```

- Keep the macro module thin and put behaviour in plain functions that take their inputs as arguments. Only one test may
  touch `Settings::current()` (it is a process-wide static); everything else tests the functions directly.
- Logging: `debug!`, `info!`, `warn!` from the prelude. Never log a secret or anything derived from one. Lines are rate
  limited (100 per second) and cut at 4096 bytes.
- HTTP: `http::Request::post(url).header(..).timeout(..).body(..).send()`. The host never follows redirects for you,
  has a 30 s attempt timeout and does not retry; retry decisions belong to the outbox (notifications) or the plugin.
  Build error strings from a status or a fixed phrase: a transport error text can contain the URL.
- Fail open by default: a hook that errors leaves the data as it was. Only opt into `resources.fail_mode: abort-target`
  for output that must not be skipped.
- Hooks must be deterministic: the output is a function of the input batch, the settings, the KV state and the host
  clock. Read time only with `clock::now_unix()` (the SDK has no `SystemTime` shim on purpose), and cache a network
  result the output depends on in the KV store (see `docs/src/plugins/host-api.md`, determinism contract).

### Notification plugins (a message channel)

- Start from `plugins/notifiers/discord-notifier` (embeds) or `plugins/notifiers/slack-notifier` (Block Kit): same shape, two services.
  Use the SDK's `notify` module instead of writing your own: `notify::delivery_for_status` (status and `Retry-After` to a
  `DeliveryError`) and `notify::iso8601` (times).
- Hook `notifications.send`, capability `notifications` (plus `network` to call a service). The channel appears as
  `plugin.<id>`. The host applies routing, quiet hours, suppression and the retrying outbox; the plugin only formats and
  sends, then answers with a `DeliveryError`: `Transient` (retry with backoff), `RetryAfter(secs)` (a `u32` in seconds,
  cap it at 24 h), `Permanent` (dead-letter). `notify::delivery_for_status` maps HTTP statuses like this: 2xx delivered; 408 and 429 retry (honour
  `Retry-After`, seconds or HTTP date); 5xx except 501 and 505 transient; everything else permanent.
- The `Notification` has `title`, `body` (the complete plain-text fallback), `severity` (`critical` arrives as `error`) and
  `payload`, the event as JSON: `id`, `severity`, `timestamp` (Unix seconds), `instance`, `fields` (the typed payload) and
  `sections`. **Use `sections`** (`[{heading, lines}]`, plain text, no markup) to lay out detail in the service's own
  format instead of parsing `body`; when sections are present they carry the same facts as `body`.
- Respect the target service's limits by character (not bytes), escape what the service reads as markup, and cut with a
  visible `…`. Disable anything that can notify people from untrusted text (mentions, `@channel`).
- A call has 30 s in total; keep the plugin's own HTTP timeout below that (25 s).

## Build, package, install

```sh
make plugin-<id>                          # builds plugin.wasm and writes target/plugin-<id>/<id>.tlpx
bin/package-plugin.sh plugins/notifiers/<id>   # the same without the make target (output: target/plugin-examples/<id>.tlpx)
```

- Needs `rustup target add wasm32-wasip2` and `python3` (3.8 or later).
- On macOS, if linking a native build fails, set `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=cc` for that command.
- Do not point two checkouts at one `CARGO_TARGET_DIR`: workspace crates collide and you get errors from code that is not
  yours.
- To try it in a server: unpack the `.tlpx` into `<config dir>/plugins/<id>/`, add `plugins.plugins: [{id, enabled: true, settings}]`,
  enable `web_ui` (the management API is only mounted then). **Every rebuild changes the wasm hash, so the plugin
  returns to `pending_approval`**: approve it again (Plugin Manager, or `PUT /api/v1/plugins/<id>/approve` with
  `{"wasmSha256": "<hash>", "capabilities": [...]}`). If a plugin seems to do nothing, grep the server log for `is invalid`
  and `pending_approval` before you debug anything else.
- The server does not bundle any plugin yet: a plugin from this repository is installed from its `.tlpx`.

## Verify before you report done

Run from the plugin directory unless noted, using the pinned nightly named in the `Makefile` (`PINNED_NIGHTLY_TOOLCHAIN`):

```sh
cargo +<nightly> fmt                                   # the repository rustfmt.toml applies
cargo +<nightly> clippy --all-targets -- -D warnings   # no allow attributes to silence lints
cargo check --all-targets
rm -rf target                                          # cargo created it here; it must not stay in the folder
make plugin-<id>                                       # from the repository root: the wasm must build and package
git grep -i <id-or-name> backend shared frontend       # must find nothing that belongs to this plugin
git diff --check
```

Then, if you changed anything the host reads (manifest, schema, locales), load the package in a running server and look at
the Plugin Manager and the config page: the plugin must show `enabled`, its form must show translated labels and no raw
keys. Unit tests (`cargo test`, on the SDK mock host) are written for every behaviour, but run them only when the user
asks for it. Say plainly in your report what was compiled, what was run, and what was neither.

## Common traps

- A manifest hook without an export, or an export without a hook, silently makes the plugin invalid.
- `pending_approval` after every rebuild is the approval pin working; approve the new hash.
- A schema keyword outside the supported subset refuses the whole package at upload and again at load.
- A `default` on a secret refuses the package.
- Raw keys in the form (`settings.url.title`) mean the key is missing in the language file the UI uses, or the language is
  not listed in `ui.locales`.
- A `title_key` that is not in `en.json`, or a listed language whose file is missing or not flat strings, refuses the package.
- The host sorts schema properties alphabetically, so the form does not follow your key order.
- A plugin that returns a secret in an error string leaks it through the outbox and the logs; build messages without it.
