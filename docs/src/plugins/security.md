# Security Model

A plugin runs code that you did not write, so tuliprox treats it as untrusted. This page says what the admin approves, what
a plugin can and cannot do, and what the limits are. Each limit is stated as it is in the code.

## What the admin approves

Installing a plugin is a review followed by an approval. The review (`POST /api/v1/plugins`) lists:

- The capabilities the manifest asks for, each with the sentence the admin reads (`capability_description` in
  `backend/plugins/src/install.rs`).
- The hooks and events.
- The settings paths that hold secrets.
- Whether the plugin has custom UI (`customUi`).
- The RPC exports, and whether any of them asks for MCP exposure (`mcpRequested`).
- Any warnings, and whether the install replaces an installed version (`replaces`).

Approval pins three things: the wasm hash, the capability names, and the MCP flag. The pins are kept in
`<storage dir>/plugins/approvals.json`. The approve request must repeat the hash and the capability names from the
review, in camelCase (`wasmSha256`, `capabilities`, `mcp`). Otherwise it is refused with `409`. An approval therefore
never covers a package the admin did not see. If the wasm on disk or its capability list later differs from the pin,
the plugin goes back to `pending_approval` and does not run.

Capability sentences, from `backend/plugins/src/install.rs`:

| Capability | What the admin is told |
| --- | --- |
| `network` | "Unrestricted network access: any public, private or loopback HTTP(S) destination." |
| `storage` | "Persistent storage: a private key-value store of up to 50 MiB, kept until the plugin is uninstalled with its data." |
| `epg_write` | "EPG write: may add or change programmes in the EPG." |
| `image_cache` | "Image cache: may fetch images through the host image cache and serve them from this server (needs network access)." |
| `notifications` | "Notifications: may register as a notification provider and send messages through it." |
| `stream_headers` | "Stream header injection: may add HTTP headers to upstream stream requests when a client session starts." |
| `process` | "Run local programs: may start any program on this server, as the user tuliprox runs as, with arguments and input of its own choosing. Grant it only to a plugin you would trust with a shell." |

A hook that needs a capability makes the manifest invalid if the manifest does not request it: `stream.inject-headers`
needs `stream_headers`, `epg.transform-programmes` and `epg.enrich-programmes` need `epg_write`, and
`notifications.send` needs `notifications`.

## Running programs

**The `process` capability is the one that leaves the sandbox.** Once granted, a plugin may start any program on the server
that runs tuliprox, as the user tuliprox runs as, with arguments and input of its own choosing (`host::run-process`, SDK
`tuliprox_plugin_sdk::process`). Treat it like giving the plugin a shell: only approve it for a plugin whose author and
source you trust, and run tuliprox as an unprivileged user. The host cannot tell a harmless script from a harmful one.

What the host does limit, in `backend/plugins/src/process.rs`:

- The program is started directly, never through a shell, so a plugin's arguments are never parsed as shell syntax.
- The program's environment is empty except `PATH` and the variables the plugin passes. The host process's own environment
  (which can hold resolved secrets) never reaches a child.
- At most 256 arguments and 1 MiB of input; stdout and stderr are each cut to their first 64 KiB.
- The wait is 1 to 300 seconds; after that the child is killed and the call fails. A call the host drops (a plugin call
  has a 30 s deadline) kills its child too.
- On Unix the program runs in its own process group. When the program exits, times out or the call is dropped, the whole
  group is killed, so a background job it started can neither outlive the call nor hold it open until the timeout.
- Every run is logged as `plugin <id> runs the program <program>`, by program name and never by arguments, which may carry
  a credential.

Without the capability the call fails and nothing starts.

## Network

**The `network` capability is unrestricted.** Once granted, a plugin may call any HTTP or HTTPS host, public, private or
loopback, on any port. That includes services on the machine that runs tuliprox and on its local network. The host
applies no destination filter to these calls, and no allowlist: it does not restrict by host, address or port. The code
is in `backend/plugins/src/http.rs`, which deliberately leaves out the resource destination resolver that other
outbound requests use. Only `http` and `https` are allowed, and the host does not follow redirects itself, so a plugin
that follows one makes a new request that is under the same limits and equally unrestricted.

The limits that do apply are the ones in [Host API](./host-api.md#outbound-http): a 30-second attempt timeout, a 35-second
operation deadline, 16 MiB request and response bodies, 64 headers, 8 concurrent requests per plugin, a rate of 20 per second,
and a circuit breaker per plugin. The host does not follow redirects itself. Each hop is a separate request under those
limits.

Any plugin with `network` can reach your local network, including services that are not meant to be public. Grant it only to
plugins you trust.

## Storage

The `storage` capability gives a key-value store of 50 MiB per plugin. Keys are at most 1024 bytes, values at most 1 MiB.
The store is private to the plugin. Uninstalling the plugin keeps the store unless the request asks for `purge_kv=true`.

## Custom UI

A plugin with custom UI runs its scripts in a sandboxed frame. The frame has an opaque origin, `connect-src 'none'` and
`sandbox allow-scripts`. The full policy is in [Custom UI](./custom-ui.md#sandboxed-frames). What it means:

- The frame cannot read the admin token, the page's storage or its DOM.
- The frame cannot make network requests. Its only channel is `postMessage` to the admin page, which performs any
  authenticated call itself, and only for methods the manifest declares.
- The frame document is served without authentication. That is a design choice: an iframe `src` carries no token. The
  route serves only files under the plugin's `ui/` directory, of an allowed type, for an enabled plugin.
  Declared locale JSON files are also available for loaded, disabled and failed plugins so settings can be translated
  before enabling; executable UI assets still require an enabled plugin.

Limits that remain:

- **Navigation.** The content security policy does not cover navigation. A frame can navigate itself, and the request
  that results can carry anything the frame holds, to any address. So the frame is not a place to put a secret. Slot
  contexts therefore carry no stream or catch-up URLs, which embed provider credentials. They carry channel and
  programme names and ids, nothing that is a credential.
- **Methods.** An approved plugin UI can call whatever its declared `rpc_exports` allow. The install review says custom UI is
  present, and the admin should read the methods.

The admin page never runs plugin JavaScript outside the frame. Schema-driven settings forms involve no plugin JavaScript.

## Secrets

- A secret is a schema property with `writeOnly: true` or `format: password`. It must not have a default.
- A literal secret is masked as `********` when the settings are read back, and a masked value sent back keeps the
  stored value.
- A `${env:VAR}` reference is resolved when the config is loaded. Prefer it for every secret.
- The approval review lists the secret paths, so the admin sees "this plugin has an API token" before approving.
- The resolved value reaches only the plugin that owns it, through `get-config`. The host replaces it with `********`
  in the plugin's log lines, in task results and in rpc results on their way to the UI frame. A channel's `extra` bag is
  built from the item's own fields, not from settings. The match is on the exact text, so a secret that the plugin has
  encoded or cut is not recognised. A plugin that sends a secret to a host it can reach, or writes it into a playlist
  field, is not stopped by this. A plugin with `network` is trusted with its own secrets.
  (`backend/plugins/src/logs.rs`, `rpc.rs`, `tasks.rs`.)

## Trust on first use

**A config entry is an approval.** If `config.yml` has a `plugins.plugins[]` entry for an id with `enabled: true` (the
default when `enabled` is left out), and no approval is recorded for that id, the host approves whatever plugin is on
disk under that id, with the capabilities its manifest asks for, at the next load. There is no review. The host logs
``plugin `<id>` approved by its config entry (trust on first use), capabilities: …`` as a warning, naming the
capabilities it granted, and writes the pin
(`backend/plugins/src/registry.rs`). The demo configs of the example plugins rely on this.

From then on the pin applies: a changed `plugin.wasm` or capability list puts the plugin back to `pending_approval`.

The pins live in `<data dir>/approvals.json`. Losing that file is the same as never having approved: trust on first use
applies again on the next load. Anyone who can delete it can also edit it, so this grants nothing beyond write access to
the data directory, which you should protect like `config.yml`. An `approvals.json` that exists but cannot be read or
parsed is different: tuliprox then approves **nothing** by config entry, logs an error, and leaves the file as it is.
Every plugin without a matching pin waits for approval. The next approval in the Plugin Manager moves the unreadable
file aside as `approvals.json.unreadable-<unix seconds>` and starts a new one.

Do not write an enabled entry, and do not copy a plugin directory into `<config dir>/plugins/`, for a plugin you have not
reviewed. Upload, review and approve the package instead. A settings save for an id with no entry creates the entry
disabled, for this reason (`backend/app/src/api/endpoints/plugins_settings_api.rs`).

## Integrity

- The wasm hash is pinned at approval. A changed `plugin.wasm` on disk puts the plugin back to `pending_approval`.
- Package signatures are not supported. The code has no signature check, so the hash pin is the only integrity
  mechanism. It protects against a change after approval, not against a package that was bad when you approved it.
- The upload route unpacks defensively: no symlinks, no `..` or absolute paths, size and entry limits, and files are
  created with `create_new`. See [Packaging](./packaging.md).

## Failure containment

- A plugin that traps, runs past its deadline, exhausts its memory or returns an error is treated as failed for that call. Its
  data is left as the previous plugin left it.
- Five consecutive failures trip the plugin's circuit breaker. A tripped plugin is skipped until it is enabled again or
  the registry reloads. Notifications to a tripped plugin's channel are rejected permanently while it is not loaded.
- `resources.fail_mode: abort-target` makes a failure abort the target, instead of continuing without the plugin's change.
- Header injection for a stream runs under a 50-millisecond clamp, is cached, is fail-open, and cannot set connection-control
  headers.

## MCP

A plugin tool can reach an AI assistant. The assistant receives the plugin's output as data, and the output can contain text
aimed at the assistant. The host does not try to interpret that text. Exposure needs three switches: `mcp: true` on the
export, the admin's `mcp` flag at approval, and `mcp.enabled` with `mcp.plugin_tools` in `config.yml`. Write tools are
hidden in read-only mode, and the MCP client should confirm write calls.

See [Events, Notifications, Inputs, Outputs, Tasks, Metrics and MCP](./events-notifications-inputs.md#mcp-tools).
