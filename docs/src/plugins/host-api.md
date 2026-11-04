# Host API

A component reaches the host only through the imports of its world. The host interface is `host` in
`plugins/plugin-api/wit/tuliprox-plugin.wit`. The outbound HTTP import is `wasi:http/outgoing-handler@0.2.12`. Everything else
the SDK offers is built on those imports. The component has no file system or socket of its own.

## Host functions

| Function | Signature (WIT) | Notes |
| --- | --- | --- |
| `log` | `log(level, msg)` | Buffered per plugin, see [Logs](#logs) |
| `kv-get` | `kv-get(key) -> result<option<list<u8>>, kv-error>` | Needs capability `storage` |
| `kv-set` | `kv-set(key, list<u8>) -> result<_, kv-error>` | Needs capability `storage` |
| `kv-delete` | `kv-delete(key) -> result<_, kv-error>` | Needs capability `storage` |
| `get-config` | `get-config() -> string` | The plugin's settings as JSON, with `${env:VAR}` references resolved |
| `now-unix` | `now-unix() -> s64` | Unix seconds from the host clock. See [Determinism](#determinism-contract) |
| `cache-image` | `cache-image(url) -> result<string, string>` | Needs capabilities `image_cache` and `network`. Returns a URL served by this server |
| `run-process` | `run-process(program, args, env, stdin, timeout-secs) -> result<process-output, string>` | Needs capability `process`. Starts a local program and waits. See [Running programs](./security.md#running-programs) |

The `kv-error` cases are `quota-exceeded`, `key-too-large`, `value-too-large`, and `unavailable(string)`.

Without a capability the call is refused at run time, not at load time: a KV call without `storage` returns
`unavailable("the plugin has no storage capability, so it has no KV store")`, and `cache-image` without both
capabilities returns an `err` string. `get-config` is not gated. It returns the plugin's own settings with secrets
resolved, and only to that plugin's own instances (`backend/plugins/src/host_fns.rs`).

`cache-image` also returns `err`, and the plugin keeps the upstream URL, when the server's resource cache is disabled,
when the image is over 2 MiB, when the plugin's 64 MiB image quota is spent, or when the type is `image/svg+xml`
(`backend/plugins/src/image_cache.rs`). In CLI mode there is no resource cache, so it always returns `err`.

`run-process` returns `err` without the `process` capability, for a malformed request (an empty program, more than 256
arguments, over 1 MiB of input, a NUL byte, an invalid environment name), when the program cannot be started, and when it
is still running after `timeout-secs` (1 to 300; it is then killed). A program that ran and failed is `ok` with its
`status`; `status` is `none` when a signal ended it (`backend/plugins/src/process.rs`).

The SDK wraps these functions. The names below are those of `plugins/plugin-sdk/src/`:

- `log` with `trace`, `debug`, `info`, `warn` and `error` macros, which take any `AsRef<str>`.
- `kv` with `get`, `set`, `delete`, `get_str`, `set_str`, `get_json`, `set_json`, `set_ttl`, `get_fresh`, `set_json_ttl`
  and `get_json_fresh`.
- `images::cache(url)`, `clock::now_unix()` and `config::raw()`.
- `http` with `Request` builders (`get`, `post`, `put`, `delete`, `header`, `bearer`, `body`, `json`, `timeout`,
  `send`), a `Client` with defaults, `Response` with `status`, `header`, `text`, `json` and `error_for_status`, and
  free functions `get` and `get_json`. It is built on `wstd` 0.6.8 and runs with `block_on` inside synchronous exports.

The `kv` TTL helpers store an expiry in front of the value, using `now-unix`. Read a value written with `set_ttl`
with `get_fresh` (or the JSON twin), not with `get`.

## Storage limits

From `backend/plugins/src/kv.rs`:

| Limit | Value |
| --- | --- |
| Quota per plugin | 50 MiB |
| Maximum key | 1024 bytes |
| Maximum value | 1 MiB |

Uninstalling a plugin keeps its KV data unless the uninstall request asks for `purge_kv=true`.

## Logs

From `backend/plugins/src/logs.rs`:

- A plugin keeps the last 1000 log lines. `GET /api/v1/plugins/{id}/logs` returns them. The query takes a minimum
  level, a start time and a limit.
- Lines are rate limited to 100 per 1000 ms window, across all instances of the plugin. Lines over the limit are
  dropped and counted, and one warning with the count is logged when the window closes.
- A message longer than 4096 bytes is truncated and marked `…[truncated]`.
- Every line is also written to the server log, with the target `plugin::<id>`.
- Resolved secret values are replaced with `********` in the line before it is buffered or logged. This is an exact
  text match, so a secret that the plugin has transformed (encoded, split, cut) is not recognised. Never log a secret.

## Outbound HTTP

Source: `backend/plugins/src/http.rs`.

| Limit | Value |
| --- | --- |
| Attempt timeout | 30 seconds |
| Operation deadline | 35 seconds |
| Maximum request body | 16 MiB |
| Maximum response body | 16 MiB |
| Maximum request headers | 64, and 16 KiB of header bytes |
| Concurrency | 8 requests at a time per plugin |
| Rate | 20 per second, burst 40 |
| Circuit breaker | opens after 5 failures, for 30 seconds |

Each plugin has its own executor, so one plugin's failures do not exhaust another's limits.

The client never follows redirects. A 3xx response reaches the guest, and each hop is a separate request under the same
limits. Guest request-option timeouts can tighten the limits, never widen them.

The host does not retry. The guest's HTTP library decides that.

The client uses the host's proxy and TLS settings. It does not apply the destination resolver that filters resource
requests. The network capability is described in [Security Model](./security.md).

## Time and CPU

- Hook calls have a deadline set by `plugins.max_exec` (default `5s`), clamped by the manifest's `resources.max_exec_ms`.
- Scheduled task runs have a deadline set by `plugins.max_task_exec` (default `10m`), clamped by the task's `max_exec_ms`.
- The engine checks its epoch every 10 ms. The wall-clock limit has one second of slack beyond the CPU deadline
  (`backend/plugins/src/runtime.rs`).
- `stream.inject-headers` runs under a 50 ms clamp for the whole step, see [Hook Reference](./hooks.md).
- Input and output provider calls run under ten times the hook deadline, never above `plugins.max_task_exec`.

## Memory and instances

- A plugin's memory is `resources.max_memory_mb` in its manifest, clamped to `plugins.max_memory` (default `128MB`).
- All instances together use at most `plugins.max_total_memory` (default `512MB`).
- At most `plugins.max_instances` instances of one plugin run at once (default 4). A manifest's
  `resources.max_instances` is clamped to that ceiling (`backend/plugins/src/runtime.rs`).

## Plugin directories and search order

The `plugins.directories` list specifies the search directories holding plugins. In Docker, this defaults
automatically to `[/app/plugins, /app/user-plugins]`:

```yaml
plugins:
  enabled: true
  directories:
    - /app/plugins       # Bundled official plugins
    - /app/user-plugins  # Custom or user-mounted plugins
```

Directories are searched in the order listed. If multiple directories contain a plugin with the same ID, the later
directory takes precedence (overrides earlier entries). The last entry in `directories` serves as the target directory
for plugin installations and uploads via the Web UI.

## The `plugins:` limits

The host limits above are set in the `plugins:` block of `config.yml` (`shared/src/model/config/plugins.rs`). A
manifest's `resources:` can only ask for less.

| Key | Default | Meaning |
| --- | --- | --- |
| `max_total_memory` | `512MB` | All instances of all plugins together |
| `max_memory` | `128MB` | One instance. The manifest's `resources.max_memory_mb` is clamped to it |
| `max_exec` | `5s` | One hook call. The manifest's `resources.max_exec_ms` is clamped to it |
| `max_task_exec` | `10m` | One scheduled task run |
| `stream_header_ttl` | `60s` | How long injected stream headers are reused |
| `max_instances` | `4` | Concurrent instances of one plugin |
| `batch_size` | `500` | Items per hook call, at most 10000 |

Each plugin entry in `plugins.plugins[]` also has `priority` (default 100, lower runs first).

`plugins.repositories[]` lists the marketplace sources:

```yaml
plugins:
  repositories:
    - id: community
      name: Community plugins
      url: ${env:TP_MARKETPLACE_URL}   # the repository's marketplace.json
      enabled: true
```

`id` follows the plugin id shape (lowercase letters, digits and `-`, 3 to 41 characters) and is unique. `name` and
`url` must not be blank. `url` is the direct `https://` address of the repository's `marketplace.json`, and may be a
`${env:VAR}` reference: the reference is what is stored and shown, the value is looked up when the source is used. Other
schemes, an unset reference and a url that is not valid are config errors that name the source, at startup and on
a config reload. `enabled` defaults to `true`. Removing a source leaves the plugins installed from it in place.

## Rpc and tasks

- An rpc request body is at most 64 KiB. The result is at most 256 KiB. Rate is 10 per second with a burst of 20
  (`backend/plugins/src/rpc.rs`).
- A task result is at most 2048 bytes (`backend/plugins/src/tasks.rs`).

## SDK helpers

- `metrics!{ "key", "Label" => value, unit = "x", severity = Warn; }` builds the `collect-metrics` result.
- `events::EventExt::typed()` decodes an event payload into `TypedEvent` variants. See
  [Events, Notifications and Providers](./events-notifications-inputs.md).
- `tasks::Router` dispatches a `tasks.run` call on the task id.
- `rpc::Router` dispatches a `rpc.call` on the method name.
- `notify::delivery_for_status(status, retry_after)` turns an HTTP status and a `Retry-After` header into the `DeliveryError`
  of a notification plugin (2xx delivered, 408 and 429 retried, 5xx except 501 and 505 transient, the rest permanent).
  `notify::parse_retry_after` reads the header in seconds or as an HTTP date (at most 24 h) and `notify::iso8601` writes a
  Unix time as `YYYY-MM-DDTHH:MM:SSZ`.
- `notify::sections_of(payload)` reads the event's channel-neutral `sections` (`Section { heading, lines }`) out of a
  notification's `payload`, and `notify::markdown::{sections, lines, escape_line}` turn them, or plain text, into escaped
  markdown for services that render it: bold headings, hard line breaks, and a leading `>`, `#`, `-`, `+` or `1.` escaped
  so event text cannot become a quote, a heading or a list.
- `Secret<T>` wraps a secret value. Its `Debug` and `Display` print `********`, and it does not implement `Serialize`.
  `expose()` gives the plain value.

## Mock host for unit tests

The SDK's `mock` module replaces the host on non-wasm32 targets. Its functions are `reset`, `set_config`, `set_now`,
`advance`, `set_kv_unavailable`, `kv_snapshot`, `logs`, `set_cached_image`, `cached_images`, `http_respond`,
`http_respond_with`, `http_fail` and `http_requests`. The template's unit test uses `mock::reset` and `mock::set_config`, then calls
`plugin::Settings::reload()` because a unit test has no `init`. Run it with `cargo test` in the rendered plugin
directory. No tuliprox server is needed.

## Determinism contract

A hook's output must be a function of its input batch, its settings, its KV state and the host clock. The wall clock
is not an input: the SDK has no `SystemTime` shim on purpose. Read time with `clock::now_unix()`, and cache any
network result the output depends on in the KV store, so a second run sees the same data. KV writes count as a plugin
change: they feed the plugin fingerprint.

The host builds a fingerprint per plugin: a digest over its id, version, wasm hash, resolved settings and ABI. The
processing report (`PipelineStats.plugins`) carries the digest, so a change of plugin or setting shows up as a change
of fingerprint. `GET /api/v1/plugins/{id}/stats` shows the version, wasm hash and ABI.

For determinism checks only, the server reads the environment variable `TULIPROX_PLUGIN_FROZEN_CLOCK=<unix seconds>`
and freezes the plugins' `now-unix` (and a task's `scheduled-at`) at that time. Host-side timing, such as the metrics
throttle, the header TTL cache and log timestamps, keeps the wall clock. It logs a warning at start. Do not set it in
normal operation. The
check is: start the server twice on the same input and config with the same frozen clock, and compare the outputs
byte for byte. `plugins/examples/hf-epg-matcher/demo/CHECKS.md` (Check 3) and
`plugins/examples/sports-enricher/demo/CHECKS.md` (section 6) spell it out for two examples.

For crash-recovery testing only, the environment variable `TULIPROX_PLUGIN_TEST_CRASH_AT=<stage>` aborts the process at a
named stage of a package replacement (an approved update, a rollback, or a boot update): `journaled`, `prior_retained`,
`pin_swapped`, `entered`, `reloaded`, `checked` or `committed`. The next start then recovers the plugin from its activation
journal before plugins load. The server logs a WARN at start and right before it aborts when the variable is set. It is
read from the environment only, and it is never for production.

### Local test marketplace

To try a marketplace on your machine, serve `marketplace.json` and the packages over plain HTTP on localhost and start
the server with `TULIPROX_PLUGIN_MARKETPLACE_ALLOW_HTTP=1`. The variable is read on every fetch. It permits `http://`
catalog and package URLs and local or loopback destinations, which are refused otherwise, and it logs a warning each
time. It is for development only: do not set it in normal operation. See
[Publishing to a Marketplace](marketplace-publishing.md) for the format.
