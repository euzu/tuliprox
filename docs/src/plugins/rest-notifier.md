# REST Notifier

`rest-notifier` is a notification plugin that sends every tuliprox notification to any HTTP endpoint: a webhook, an automation
tool, your own service. It is a regular plugin: install and approve it in the Plugin Manager, then configure it under
`plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: rest-notifier
      enabled: true
      settings:
        url: "https://my-api.local/alert"
        method: POST                        # POST (default), PUT or PATCH
        body: event                         # event (default) or text
        headers:
          - "X-Environment: production"
        secret_headers:
          - "Authorization: Bearer ${env:ALERT_TOKEN}"
        signing_secret: "${env:ALERT_SIGNING_SECRET}"   # optional, see Signing
        routing:
          notify_on: [ "playlist.*", "recording.*" ]
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.rest-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying outbox
apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting          | Default | Description                                                                                                                                                |
| ---------------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `url`            | -       | The endpoint that receives each notification (`http://` or `https://`).                                                                                    |
| `method`         | `POST`  | `POST`, `PUT` or `PATCH`.                                                                                                                                  |
| `body`           | `event` | `event`: the whole event as JSON. `text`: the plain-text body only.                                                                                        |
| `headers`        | -       | Extra headers, one `Name: value` per entry. A `Content-Type` here replaces the default.                                                                    |
| `secret_headers` | -       | Headers whose value is a credential, for example `Authorization: Bearer ...` or `X-Api-Key: ...`. The whole entry is write-only and masked when read back. |
| `signing_secret` | -       | Optional. Adds a signature header (see Signing). Write-only and masked when read back.                                                                     |
| `routing`        | -       | The per-channel routing block, applied by tuliprox before the plugin is called.                                                                            |

Use `${env:VAR}` inside any of them to keep a credential out of the config file.

## What the endpoint receives

With `body: event` the request body is the event as JSON, with `Content-Type: application/json`:

```json
{
  "id": "playlist.update.completed",
  "severity": "info",
  "timestamp": 1700000000,
  "instance": "tv-box",
  "title": "Playlist update finished: 1 input(s), 1 target(s)",
  "body": "demo-in (M3u): 1 group(s), 2 channel(s), 0 error(s)",
  "fields": { "...": "the typed payload of the event" },
  "sections": [ { "heading": "Sources", "lines": [ "..." ] } ]
}
```

`severity` is `info`, `warn`, `error` or `critical`; `instance` appears when configured; `sections` appears when the event has
detail beyond its body. With `body: text` the request body is the plain-text `body` with `Content-Type: text/plain; charset=utf-8`.

There are no templates. A body shaped for one particular service (a chat webhook that wants its own JSON) belongs in a notifier
plugin for that service; see `plugins/AGENTS.md` for how to write one.

## Signing

With a `signing_secret` every request carries:

```text
X-Tuliprox-Timestamp: 1700000000
X-Tuliprox-Signature: sha256=<hex>
```

The signature is the hex HMAC-SHA256 of `{timestamp}.{body}` under the shared secret, over the exact bytes sent. The timestamp is
inside the signed payload, so a captured request cannot be replayed later under a fresh timestamp header. Verify it by recomputing
the HMAC and comparing in constant time, and reject requests whose timestamp is too far from your clock. A header of the same
name in the settings is ignored while signing is on, so the receiver never sees two.

## Delivery

| The endpoint answers                                                                            | Result                                                           |
| ----------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| 2xx                                                                                             | delivered                                                        |
| 429, 408                                                                                        | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                                                                          | retried with backoff                                             |
| 501, 505, 3xx and every other 4xx                                                               | dead-lettered                                                    |
| no answer within 25 s                                                                           | retried with backoff                                             |
| `url` missing or not http(s), a malformed header entry, a body over 1 MiB, more than 64 headers | dead-lettered, nothing is sent                                   |

The built-in channel guarded the endpoint with its own circuit breaker, rate limit and concurrency cap. A plugin's HTTP is under
the host's per-plugin limits instead (a circuit breaker that opens after 5 failures for 30 seconds, 8 requests at a time, 20 per
second), so the endpoint is protected the same way. The redirects of the endpoint are not followed.

Secret headers and the signing secret appear in no log line or error message. A malformed `headers` or `secret_headers` entry is
reported by its position in the list, never by its text.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-rest-notifier` writes `target/plugin-rest-notifier/rest-notifier.tlpx`, which can be uploaded in the Plugin Manager.
