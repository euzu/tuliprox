# Matrix Notifier

An example tuliprox plugin that delivers notifications to a Matrix room. It is a
source example only: it is not shipped in the Docker image and not offered in the
marketplace. The built-in messaging channels (Telegram, Discord, Slack, Gotify, ntfy,
Pushover, REST, command) are the supported way to notify; Matrix has no built-in channel,
so build and install this plugin if you need it. It is
the reference for a **notification provider**: the plugin adds a delivery channel
(`plugin.matrix-notifier`) to the messaging pipeline, and everything else (event
subscription, severity floors, quiet hours, suppression, hourly ceilings, the
persisted outbox with backoff and dead-lettering) is the host's.

| Piece | Mechanism |
| --- | --- |
| Deliver a notification to a room | hook `notifications.send`, capability `notifications` + `network` |
| Idempotent retries | deterministic Matrix transaction id, remembered in the KV (`storage`) until the message is accepted |
| Retry / dead-letter decisions | the returned `DeliveryError`: `transient`, `retry-after` (from `retry_after_ms`), `permanent` |
| Severity floors, quiet hours, suppression | `settings.ext.messaging.routing`, read by the host; no plugin code |
| React to bus events | hook `events.on-event` for `playlist.update`, `user.connection.changed`, `stream.probe.failed`: log + KV counters only |
| Settings form | the `messaging` fragment of `ui.config_extensions` in `manifest.yml` (rendered on the Web UI messaging page); no standalone settings form, no UI code |

## Delivery contract

`PUT {homeserver}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn_id}` with
`Authorization: Bearer <access_token>` and `{msgtype: "m.text", body, format:
"org.matrix.custom.html", formatted_body}`.

| Matrix answered | Plugin returns | Host does |
| --- | --- | --- |
| 2xx | `Ok` | settles the notification |
| 429 | `retry-after(ceil(retry_after_ms/1000), 1..3600 s; header, else 5 s)` | retries after that delay |
| 5xx, connection error, timeout (4 s) | `transient` | retries with exponential backoff |
| any other status, bad URL, missing room/token | `permanent` | dead-letters that channel, no retry |
| anything below `min_severity` | `Ok`, nothing sent | counts it delivered |

Two severity floors exist and the host's runs first: `ext.messaging.routing.min_severity`
is judged by the host on the event's registry severity and the plugin is not even
called below it; the plugin's `min_severity` setting is checked inside the plugin.

**Transaction ids.** The id is `tuliprox-<128-bit hash of title/body/severity/payload>-<unix
time of the first attempt>`. The first attempt time of a message that was not yet accepted is
kept in the KV (`txn-index`), so every retry of the same notification sends the same id and the
homeserver stores one event even when an acknowledgement was lost. The record is dropped when
the message is accepted or refused for good, so a later identical notification is a new message.
(Hashing `now` into the id on every attempt would give each retry a different id and defeat the
point; only the first attempt's time is used.)

## Settings

All keys live under `settings.ext.messaging` of the plugin entry:

```yaml
plugins:
  enabled: true
  plugins:
    - id: matrix-notifier
      settings:
        ext:
          messaging:
            room_id: "!abc:example.org"
            access_token: "${env:MATRIX_TOKEN}"
            routing:
              min_severity: warn
```

| Key | Meaning |
| --- | --- |
| `homeserver` | base URL, default `http://127.0.0.1:18972` (the demo mock) |
| `room_id` | internal id (`!abc:example.org`), required |
| `access_token` | bot token, required, write-only secret (`${env:VAR}` works) |
| `min_severity` | `info` (default), `warn`, `error` |
| `routing` | host-side routing block: `notify_on`, `min_severity`, `quiet_hours`, `max_per_hour`, `dedup_window_secs` |

The messaging block must subscribe to events with `notify_on`, exactly as for a built-in
channel. Notification ids are not bus event names: a refresh is `playlist.update.completed`
or `playlist.update.failed`.

## Build, mock and demo

```sh
bin/package-plugin.sh plugins/examples/matrix-notifier        # target/plugin-examples/matrix-notifier.tlpx
python3 -I plugins/examples/matrix-notifier/mock/matrix_hs.py --port 18972   # mock homeserver; GET /_stats
```

The mock checks the bearer token and room, stores messages idempotently by transaction id and
can fail (`--fail 503`, `--fail-after-store 503`, `--rate-limit`, or `POST /_mode` at run time)
or be killed. `demo/` holds a complete server-mode config, an M3U and `demo/CHECKS.md` with the
steps and expected results.

Unit tests (`cargo test --manifest-path plugins/examples/matrix-notifier/Cargo.toml`) run on the
SDK mock host: status mapping, transaction id determinism, `min_severity` skip, request shape,
on-event counters.

## Determinism

Hooks depend only on their input, the settings, the KV and `clock::now_unix()`. The message text,
the fingerprint and the transaction id are pure; the KV holds `BTreeMap`s.
