# WebAssembly Plugins

A plugin is a WebAssembly component that runs inside tuliprox. It can change playlists and EPG data, supply
inputs and outputs, send notifications, run scheduled work, expose settings and a sandboxed admin page, and
answer calls from the web UI and MCP clients.

When the server starts and `config.yml` has no `plugins:` section, it appends a minimal one (`plugins: {enabled: true}`)
so the Plugin Manager works. The section is added once as plain text; your comments and values stay as they are, and a
section you wrote is never changed. Set `plugins.enabled: false` to turn the plugin system off: no plugin engine is
created and the pipeline behaves as it does without the plugin system. If the file cannot be written (a read-only
mount), the server logs a warning and starts with plugins off. The management API (`/api/v1/plugins`) is part of the web API, so it needs
`web_ui.enabled: true`. To build and run a first plugin, start with the [Quickstart](./quickstart.md).

Notification delivery uses plugins. Install and approve a notifier from the Marketplace, configure it in the Plugin Manager,
and enable it. The `messaging` configuration controls global event subscriptions and disk alerts; transport credentials
and per-channel routing belong to each notifier's settings. Built-in channel keys such as `messaging.telegram` and
`messaging.discord` are no longer accepted.

## What a plugin is

A plugin is shipped as a `.tlpx` package, which is a zip archive with these parts:

| Part | Required | Purpose |
| --- | --- | --- |
| `manifest.yml` | yes, at the zip root | Identity, capabilities, hooks, events, tasks, scope, UI, resource limits |
| `plugin.wasm` | yes, when `hooks:` is not empty | The WebAssembly component (`wasm32-wasip2`) |
| `config-schema.json` | optional | JSON Schema of the plugin settings, shown as a form |
| `ui/` | optional | Frame documents, scripts, styles and `ui/locales/<lang>.json` |
| `rpc/*.json` | optional | Input schemas of `rpc_exports` |

The package layout and limits are in [Packaging](./packaging.md). A plugin is built from the SDK crate
`tuliprox-plugin-sdk` (`plugins/plugin-sdk/`) in Rust, from `plugins/examples/template`. The repository has three more
examples: `sports-enricher`, `hf-epg-matcher` and `matrix-notifier`.

`tuliprox-plugin-api`, `tuliprox-plugin-sdk` and `tuliprox-plugin-sdk-macros` have their own version (1.x), the
version of the plugin contract. A tuliprox release does not change it, so a plugin's `Cargo.lock` stays valid across
tuliprox updates. The three crates always share one version: `make plugin-sdk-version VERSION=x.y.z` sets it on all
three and refreshes the `Cargo.lock` of every plugin in the repository, and `make architecture-check` fails when they
drift apart.

The manifest declares what the plugin does. The component exports the matching interfaces. The host checks both
sides: a manifest hook without an export, or an export without a manifest hook, makes the plugin invalid. Unknown
top-level keys in the manifest are ignored.

Minimal manifest from the template, `plugins/examples/template/manifest.yml`:

```yaml
id: "{{project-name}}"        # ^[a-z][a-z0-9-]{2,40}$, unique on the host
name: My plugin
version: 0.1.0               # semver, the plugin's own version
api_version: 1               # host ABI major version
category: playlist           # what kind of plugin it is: notifier, playlist, epg, ... (optional)
author: Your Name <you@example.com>
description: >
  Renames one playlist group in the after-filter stage. Replace this text.

hooks:
  - playlist.after-filter    # the component must export playlist-hooks for this id
```

## Worlds and hooks

The contract is the WIT package `tuliprox:plugin@1.0.0` (ABI 1.0.0) in `plugins/plugin-api/wit/tuliprox-plugin.wit`. Every
component imports the `host` interface and `wasi:http/outgoing-handler@0.2.12`, and exports `lifecycle` plus the hook
interfaces it implements. The file defines one world per hook interface (`playlist-plugin`, `epg-match-plugin`,
`notifier-plugin` and so on, all including `base`), and one composite, `enricher-plugin`. The host does not assume a
world: it binds each interface by name and finds the exports when it loads the component.

Each supported hook id in `hooks:` maps to exactly one interface. The contract defines 28 hook ids; 27 are currently
supported. A manifest that declares the reserved `epg.resolve-conflict` hook is rejected until its host integration is
implemented. [Hook Reference](./hooks.md) lists the hooks with their timing and requirements.

Capabilities are the permissions an admin grants. The manifest names them under `capabilities:`:
`network`, `storage`, `epg_write`, `image_cache`, `notifications`, `stream_headers` and `process`. Nothing is granted by
default. See [Security Model](./security.md).

## Fail-open

By default a plugin that fails leaves the data as the previous plugin left it. A failure is a trap, a deadline, an
out-of-memory condition, an `err` result or a lack of budget. The failure is logged, counted on the plugin's
circuit breaker, and the next plugin runs. Source: the module documentation of `backend/plugins/src/dispatch.rs`.

Five consecutive failures trip the plugin's circuit breaker: the plugin is skipped until an admin enables it again
or the registry reloads.

A plugin can opt into `resources.fail_mode: abort-target` in its manifest. Then a failure aborts the target that
was being processed. Use it only for a plugin whose output must not be skipped, such as a dedupe rule. The template
uses `fail_mode: continue`.

An invalid plugin also fails open. The server keeps running without it and logs
``plugin `<id>` is invalid: <reason>`` at warning level. When a plugin has no effect, search the log for `is invalid`
first.

## Lifecycle

Each plugin has one state. The states are the values of `PluginStateDto` in `shared/src/model/plugins_dto.rs`:

| State | Meaning |
| --- | --- |
| `discovered` | Found in the plugins directory, not yet evaluated |
| `invalid` | The manifest or the wasm failed validation. `failure` says why |
| `pending_approval` | Not approved yet, or the wasm hash or capabilities changed since approval |
| `approved` | Approved, not loaded |
| `loaded` | Compiled and linked, not enabled |
| `enabled` | Running |
| `disabled` | Switched off by the admin |
| `failed` | The circuit breaker tripped. Enabling it again re-arms it |

The normal path is upload, approve, configure, enable:

1. **Upload.** `POST /api/v1/plugins` with the `.tlpx` as multipart field `file`. The package is validated and staged.
   Nothing is installed. The answer is a review: capabilities, hooks, secrets, custom UI, MCP request.
2. **Approve.** `PUT /api/v1/plugins/{id}/approve` pins the reviewed wasm hash and capability names, and installs the
   package under `<config dir>/plugins/<id>/`. Approval does not enable the plugin, except when `config.yml` already
   holds a `plugins.plugins[]` entry for it — that entry defaults to `enabled: true` and is an approval of its own.
   A body that differs from the review is refused with `409`.
3. **Configure.** `PUT /api/v1/plugins/{id}/settings` saves the settings. They are checked against the schema first.
4. **Enable.** `PUT /api/v1/plugins/{id}/enable` switches the plugin on. Enabling a plugin that is `pending_approval`
   or `invalid` is refused with `409`.

All of these routes need the `SystemWrite` permission when web auth is on. With no `web_ui.auth` block there is no
auth.

There is a second way in: unpack a package into `<config dir>/plugins/<id>/` and write a `plugins.plugins[]` entry
with `enabled: true` in `config.yml`. With no approval recorded for that id, the host approves the plugin on first
sight, without a review. **A config entry is an approval.** See [Security Model](./security.md#trust-on-first-use).
The demo configs of the example plugins use this path.

Disable and uninstall are `PUT /api/v1/plugins/{id}/disable` and `DELETE /api/v1/plugins/{id}`. Uninstall can also
delete the plugin's storage with `?purge_kv=true`.

**Removing a plugin never stops the server.** A `source.yml` entry that still names it is reported once at startup
(``invalid plugin references: input my-feed: no loaded plugin my-plugin provides fetch-input``) and then ignored: that
input or target is inactive and keeps its last playlist, every other input and target runs. See
[Input providers](./events-notifications-inputs.md#input-providers).

The full route list is in [Quickstart](./quickstart.md) and [Packaging](./packaging.md).

In the [Quickstart](./quickstart.md) run, `GET /api/v1/plugins` reported `loaded` after approval and `enabled` after
the enable call.

## Other languages

The host runs any component that exports the WIT interfaces in `plugins/plugin-api/wit`, so the Rust SDK is not the only way
to write a plugin. The SDK gives you the macros, settings, KV, HTTP and mock host. In another language you write the
glue yourself. The requirements are the same:

- Target WASI preview 2 and produce a component, not a core module.
- Use a world that exports `lifecycle` and exactly the interfaces your manifest lists. The shipped worlds have one
  hook interface each. For several hooks, define a small composite world in your own copy of the WIT, as
  `enricher-plugin` does. An export without a matching manifest hook makes the plugin invalid.
- Package the component with `bin/package-plugin.sh <plugin-dir> --wasm FILE`. The directory needs a
  `manifest.yml` and no `Cargo.toml`. See [Packaging](./packaging.md).

**TypeScript is verified; the others are not.** `plugins/examples/typescript-rename` was built with jco 1.40 and
loaded by the host (see its README). No component was built with TinyGo or componentize-py in this project, so
those two outlines are what each tool's documentation describes. Treat them as a starting point and expect to
adjust them.

- **TinyGo.** Generate Go bindings from `plugins/plugin-api/wit` with `wit-bindgen-go`, and build with TinyGo's
  `wasip2` target.
- **Python.** `componentize-py` builds a component from a Python module and a WIT directory. The options are
  `-d plugins/plugin-api/wit` and `-w <world>`.
- **JavaScript and TypeScript (verified).** ComponentizeJS (`jco componentize`) builds a component from a JS module
  and the WIT directory, with `--wit plugins/plugin-api/wit --world-name <world>`. `jco types` generates the
  TypeScript types. Bundle to one file first. A WIT `result` is a plain return for ok and a thrown value for err, and
  the host import is `tuliprox:plugin/host@1.0.0`, with the version. The component is about 13 MB and its hook runs
  in about a millisecond. Complete example: `plugins/examples/typescript-rename`.

<!-- UNVERIFIED: the TinyGo and componentize-py recipes. No component was built with them in this project, so the
flags above come from the tools' documentation, not from a run. -->

Incoming REST APIs are described in [Incoming HTTP routes](./http-routes.md).

Plugins can also add their own live channels to selected inputs or targets with [Managed Channels](./managed-channels.md).
