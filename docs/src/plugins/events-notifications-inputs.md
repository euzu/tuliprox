# Events, Notifications, Inputs, Outputs, Tasks, Metrics and MCP

This page covers the hooks that act outside the playlist pipeline. Each subsection names the manifest keys it needs.

## Events

A plugin receives bus events with `events.on-event`. It lists the events it wants under `events:`:

```yaml
events:
  - playlist.update
  - user.connection.changed
  - stream.probe.failed
```

From `plugins/examples/matrix-notifier/manifest.yml`.

The names are the wire names of `EventKind` in `shared/src/model/event/kind.rs`. There are 43 kinds. Any of them can be
subscribed to. These are some of them:

| Wire name | Meaning |
| --- | --- |
| `playlist.update` | A playlist update finished |
| `playlist.watch.changed` | A watched playlist changed |
| `user.connection.changed` | A user's connections changed |
| `stream.probe.failed` | A stream probe failed |
| `recording.changed` | A recording changed |
| `recording.rules.changed` | Recording rules changed |
| `scheduled_task.failed` | A scheduled task failed |

An unknown name makes the manifest invalid: the plugin is refused at upload, or marked `invalid` at load, with
`events lists unknown event names: ...` (`backend/plugins/src/manifest/validate.rs`). The retired name
`playlist.groups.changed` is still accepted and means `playlist.watch.changed`.

An event name is not a notification id. A plugin's `events:` list takes event names such as `playlist.update`. The
messaging `notify_on` list takes notification ids such as `playlist.update.completed`. See
[Notification provider](#notification-provider).

**Delivery.** The bridge (`backend/app/src/api/tasks/plugin_event_bridge.rs`) offers each event to a bounded queue of 256
entries (`backend/plugins/src/events.rs`). It never waits on a plugin. When the queue is full the event is dropped and
counted. If the bridge falls behind the event bus, it logs a warning and skips the gap. Each subscribed plugin runs in a
fresh instance for each event. A failure is logged and counted against that plugin's circuit breaker. It never reaches
the event's emitter or another plugin. Delivery is fire-and-forget: there is no retry.

**Coalescing.** `recording.changed` and `recording.rules.changed` are coalesced within a 250 ms window. A plugin subscribed to
them receives fewer events than were emitted.

**Payload.** An event has a `kind`, a JSON `payload` string and a `timestamp` in Unix seconds. Inside the component,
`events::EventExt::typed()` decodes it into `TypedEvent`. The decoding is lenient: unknown fields are ignored and an
enum value the SDK does not know becomes `Unknown`. Only five kinds have a typed payload
(`plugins/plugin-sdk/src/events.rs`):

| Wire name | `TypedEvent` variant | Payload fields |
| --- | --- | --- |
| `playlist.update` | `PlaylistUpdate` | `run_id`, `execution_order`, `state` (`Success`, `Failure`, `Partial`), `stats`, `error` |
| `user.connection.changed` | `UserConnectionChanged` | `Updated(StreamInfo)`, `Disconnected(addr)`, `DisconnectedStream {addr, uid}`, `Connections(users, connections)` |
| `stream.probe.failed` | `StreamProbeFailed` | `input`, `unique_id`, `url`, `reason` (`not_found`, `unreachable`) |
| `recording.changed` | `RecordingChanged` | none |
| `scheduled_task.failed` | `ScheduledTaskFailed` | `task`, `schedule`, `plugin_task`, `error` |

Every other kind arrives as `Other { kind, payload }` with the raw JSON.

## Notification provider

A plugin can deliver notifications. It needs capability `notifications` and the hook `notifications.send`.

Messaging treats the plugin as a channel with the id `plugin.<id>`. The channel uses the same routing, quiet hours,
suppression and outbox as a built-in channel. Source: `backend/messaging/src/channels/plugin.rs`.

- **Subscription.** The channel receives what the top-level `messaging.notify_on` list names. The list takes
  **notification ids**, not event names: a playlist refresh is `playlist.update.completed` or
  `playlist.update.failed`, and `playlist.update.*` matches both. The bus event `playlist.update` that `events.on-event`
  sees is a different thing. From `plugins/examples/matrix-notifier/demo/config.yml`:

  ```yaml
  messaging:
    notify_on:
      - playlist.update.*
      - user.connection.changed
      - stream.probe.failed
      - scheduled_task.failed
  ```

- **Routing.** Routing values come from a `routing` object in the plugin's own settings, and tuliprox applies them
  before the plugin runs. The block is read like a built-in channel's: `notify_on`, `min_severity`, `quiet_hours`,
  `max_per_hour` and `dedup_window_secs` (`plugins/examples/matrix-notifier/README.md`). An invalid block is ignored
  with a warning, and the defaults apply.
- **Severity.** `info`, `warn` and `error` map to the plugin's levels. `critical` is delivered as `error`.
- **Content.** `title` is a one-line summary and `body` the complete plain-text fallback. `payload` is the event as
  JSON: `id`, `severity`, `timestamp` (Unix seconds), `instance` (when configured), `fields` (the typed payload of the
  event) and `sections`. `sections` is the event's detail in a neutral form, a list of
  `{ "heading": "Sources", "lines": ["..."] }` blocks of plain text without markup, for example the sources and targets
  of a playlist update or the groups of a watch change. A plugin lays them out as it likes (Discord Notifier turns them
  into embed fields); `body` carries the same facts as one text. An event with no detail beyond its body has no
  `sections` key.
- **Delivery results.** The guest returns one of three `DeliveryError` values, and the host maps them as follows
  (`backend/plugins/src/notify.rs`):

  | Guest answer | Host result |
  | --- | --- |
  | `transient(message)` | Retry on the outbox schedule |
  | `retry-after(secs)` | Retry after that many **seconds** (`u32`) |
  | `permanent(message)` | Dropped, no retry |
  | a trap, a timeout or an exhausted budget | Retry on the outbox schedule |

  A refusal by the guest is not a plugin failure: it does not count against the circuit breaker. A trap or timeout does.

The outbox owns every retry. The plugin does not retry by itself, and it should not. The retry limits are the
`video.recording.notifications` settings in `config.yml` (see [DVR Operator Reference](../operator/dvr.md)). By default an entry is tried
up to 6 times, with waits of 5, 10, 20, 40 and 80 seconds between tries (`backoff_initial_secs: 5` doubling up to
`backoff_max_secs: 900`). A `retry-after` replaces the outbox's own wait for that retry. After the last
attempt the entry is dead-lettered and a `notification.dead_lettered` event is emitted.

If the plugin's circuit breaker has tripped (5 consecutive failures) when the outbox tries, the delivery is a retry:
the outbox holds it and sends it once the plugin is re-enabled or reloaded, within its usual attempt budget. If the
plugin was disabled, uninstalled or became invalid, the delivery is rejected permanently with the reason
``plugin `<id>` is not loaded``.

Make the delivery idempotent. `plugins/examples/matrix-notifier` derives a Matrix transaction id from the event, so a
retry after a lost answer does not post the message twice (`plugins/examples/matrix-notifier/README.md`).

## Input providers

A plugin can supply a source input. It needs `provides.input_type: true` and the hook `input.fetch`. In `source.yml` the
input has `type: plugin` and the id of the plugin. This is the config shape. None of the four example plugins is an
input provider, so there is no example to copy it from; the test fixture is `plugins/plugin-api/fixtures/provider-probe`.

```yaml
inputs:
  - name: my-feed
    type: plugin
    plugin: my-plugin
```

The input type is `plugin`, from `InputType` in `shared/src/model/config/input/kind.rs` (`snake_case` serde). The
`plugin:` field names the plugin id (`shared/src/model/config/input.rs`).

The plugin references in `source.yml` are checked when tuliprox starts, and a reference to a plugin that is not
installed, approved and providing that input type or output format is **reported and then ignored** — it never keeps
the server down. Removing a plugin is therefore safe: the server starts, the log names what is inactive, and only the
entries that name the missing plugin stop working.

- The log line names every problem at once, for example
  ``config source error: invalid plugin references: input my-feed: no loaded plugin my-plugin provides fetch-input``.
- That input is then inactive: it is skipped before its fetch, keeps its last playlist, and logs
  ``plugin input 'my-feed' is inactive: plugin 'my-plugin' is not installed or not enabled``. Other inputs and targets run.
- A target whose *processing filter* calls `plugin.<id>.<fn>(…)` for a missing plugin fails as a target
  (``target 'x' is inactive: its filter calls plugin 'my-plugin' …``) and keeps its previous output. Without that check
  the term would answer `false` for every item and the target would drop its whole playlist while reporting success.
- A `type: plugin_task` schedule naming a missing plugin or task is reported the same way and does not run.
- A plugin output whose plugin is missing fails only that output; the target's other outputs are written.

Saving `source.yml` through the API is stricter: `POST /api/v1/config/sources` rejects a reference to a plugin that is
not loaded (`409`/`400` with the same message), because that is an operator action with immediate feedback and the file
stays as it was. The boot path and a running server's hot reload only log.

So the order no longer matters for starting: install and approve the plugin whenever you like; the input starts working
once the plugin is enabled and the playlist is refreshed.

- The plugin's `fetch_playlist` returns the channels. `fetch_epg` returns EPG channels and programmes, or an empty list.
- For a plugin input, `url`, `username` and `password` are optional and passed to the plugin unchanged.
- Each call runs in a fresh instance with a deadline of `plugins.max_exec` times 10, never more than `plugins.max_task_exec`
  (`backend/plugins/src/input_provider.rs`).
- A failed call fails only the input that asked for it, the same as an unreachable provider.
- The Web UI source editor builds the same entry: add the **Plugin** input and pick the plugin in its **Plugin** field.
  The picker lists the installed plugins that export `input.fetch`. The editor shows only the fields every input has;
  the plugin adds its own with `ui.input_form` and reads them from `input-request.options` (see
  [Input and output forms](./settings.md#input-and-output-forms)).
- `plugin_config` holds those per-input values. The host hands them over as `input-request.options`; an input without
  one passes none.

## Output providers

A plugin can render a target output file. It needs `provides.output_format` and the hook `output.render`. Like the
input provider, this is the config shape and not an example's file:

```yaml
output:
  - type: plugin
    plugin: my-plugin
    format: my-format
    filename: my-output.txt
```

- `plugin` is required. `format` is optional, and when set it must equal `provides.output_format`.
- `filename` is a bare name in the target's storage directory. It must not start with `.` and must not contain a path
  separator. The default is `<plugin>.out`.
- `plugin_config` is the plugin's own settings for this output, passed as `output-request.options` in key order. A
  string keeps its text, any other value is its JSON text, a nested object is flattened with dotted keys.
- `filter` selects the items the output receives.
- The Web UI source editor builds the same entry: add the **Plugin** output and pick the plugin in its **Plugin** field.
  The picker lists the installed plugins that export `output.render`. `format` is not asked for: it must equal the
  plugin's `provides.output_format`, so the editor leaves it out and the host uses the plugin's own.
- `plugin_config` holds the per-output values, handed over as `output-request.options`. The plugin declares the fields
  with `ui.output_form`.

The render runs in one fresh instance per output file, under the provider deadline. The bytes returned by the plugin are
appended to a temporary file. The temporary file replaces the output only when `finish-output` succeeds. A failure removes
the temporary file and leaves the last good output in place (`backend/plugins/src/output_provider.rs`).

The host adds no timestamp or other data of its own. The same plugin, settings, options and items give a byte-identical file.

The host writes the file into the target's storage directory. This page does not describe how a client fetches it: the
output provider writes a file, and no route for serving a plugin output was traced.

<!-- UNVERIFIED: how a rendered plugin output file is served to clients. The code only writes it to the target's
storage directory (PluginTargetOutputDto, output_provider.rs); no serving route was found. -->

Source: `shared/src/model/config/target/output.rs` (`PluginTargetOutputDto`).

## Scheduled tasks

A plugin can run work on a schedule. Tasks are declared in the manifest:

```yaml
tasks:
  - id: refresh-fixtures
    title: Refresh fixture cache
    default_schedule: "0 */15 * * * *"
    max_exec_ms: 60000
    concurrency: skip
```

From `plugins/examples/sports-enricher/manifest.yml`. The fields are those of `RawTask` in `backend/plugins/src/manifest/raw.rs`:

- `id` (2 to 41 characters, lowercase kebab-case), `title`, `default_schedule`.
- `max_exec_ms` clamps the task's deadline, and `plugins.max_task_exec` (default `10m`) is the upper bound.
- `concurrency`: `skip` drops a new run while one is still going. `queue` keeps one pending run.
- `respect_maintenance` defaults to `true`.

`default_schedule` is only shown in the plugin manager as a suggestion. Nothing runs until the admin adds a schedule
entry to `config.yml`. The cron expression has six fields, with seconds first. From
`plugins/examples/sports-enricher/demo/config.yml`:

```yaml
schedules:
  - schedule: "0 */15 * * * *"
    type: plugin_task
    plugin: sports-enricher
    task: refresh-fixtures
```

The fields `plugin` and `task` are checked against the loaded plugins after load (`shared/src/model/config/schedule.rs`).

A manual run is `POST /api/v1/plugins/{id}/tasks/{task}/run`, which needs `SystemWrite`. It answers `200` with the summary,
`409` when the run is skipped, and `404` for an unknown task. `GET /api/v1/plugins/{id}/tasks` lists the tasks.

Each run uses its own fresh instance. A manual run never queues. A registry reload cancels a running task. The result
string is at most 2048 bytes, with secrets redacted, and is shown in the UI and the stats. A failed run (`Err`)
publishes `scheduled_task.failed`, with the task name `plugin:<id>/<task>`. Scheduled task runs are not exposed as MCP
tools.

## Metrics

A plugin reports dashboard metrics through `metrics.collect`, using the `metrics!` macro of the SDK. A metric has a
key, a label (a literal or a key of the plugin's `ui/locales`), a preformatted value, and optionally a unit and a
severity (`info`, `warn`, `error`) that sets the card's accent. The host polls the hook, at most every 10 seconds, and
serves a cached snapshot in between. Each plugin can report at most 64 metrics. Each metric's key is
`<plugin-id>.<key>`.

They are read from `GET /api/v1/plugins/metrics`. A plugin that traps, errors or is too slow contributes nothing in
that cycle and is counted on its circuit breaker.

## MCP tools

A plugin's `rpc_exports` can appear as tools for MCP clients. Only `ui.rpc_exports` are tools. Scheduled tasks never
are. A tool exists only when **three switches** are on, and all are off by default:

1. **The manifest.** The export sets `mcp: true` and has an `input_schema`.
2. **The admin.** The approval includes `"mcp": true` for the exact wasm that is loaded. The approve body is in the
   [Quickstart](./quickstart.md#7-approve). The install review lists the request separately (`mcpRequested`), because
   AI assistants can call these tools without a person.
3. **The server.** In `config.yml`, `mcp.plugin_tools: true`, together with `mcp.enabled: true`. See
   [MCP Server](../operator/mcp.md) for the rest of the `mcp:` block. From
   `plugins/examples/sports-enricher/demo/config.yml`:

   ```yaml
   mcp:
     enabled: true
     plugin_tools: true
   ```

The plugin must also be `enabled`. The MCP endpoint re-checks the switches on every list and every call. Switching
`mcp.plugin_tools` off removes the tools from the list.

A tool is named `plugin_<id>_<method>`, with `-` replaced by `_`, at most 64 characters of `[A-Za-z0-9_]`. The
`sports-enricher` export `get-fixtures` is the tool `plugin_sports_enricher_get_fixtures`. A name that is not usable,
or that clashes with a built-in tool or another plugin tool, is not exposed, and the clash is logged as an error that
names the plugin. A clash is not a load error. The description is prefixed with `[plugin <id>@<version>]`, has control
characters removed, and is limited to 512 characters.

A `write` method is hidden whenever MCP is read-only, and otherwise needs `SystemWrite`. The result is checked for
resolved secrets like any rpc result.

A plugin's output reaches the AI assistant as data. A compromised plugin can return text aimed at the assistant. The
admin approves MCP exposure knowing that. Use an MCP client that asks for confirmation before it calls write tools.
