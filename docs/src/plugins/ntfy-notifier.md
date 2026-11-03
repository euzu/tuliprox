# ntfy Notifier

`ntfy-notifier` is a notification plugin that publishes tuliprox notifications to an [ntfy](https://ntfy.sh) topic, on ntfy.sh
or on your own server. It is a regular plugin: install and approve it in the Plugin Manager, then configure it under
`plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: ntfy-notifier
      enabled: true
      settings:
        url: "https://ntfy.sh"          # the default; your own server works the same
        topic: "my-tuliprox-alerts"
        token: "${env:NTFY_TOKEN}"      # only for a protected topic
        style: text                     # text (default) or markdown
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.ntfy-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying outbox
apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting   | Default            | Description                                                                                          |
|-----------|--------------------|------------------------------------------------------------------------------------------------------|
| `url`     | `https://ntfy.sh`  | The base URL of the ntfy server, without the topic.                                                  |
| `topic`   | -                  | The topic to publish to. On a public server anyone who knows the name can read it, so choose one that cannot be guessed or protect it with a token. |
| `token`   | -                  | Optional bearer token for a protected topic. Write-only in the UI; use `${env:VAR}` to keep it out of the file. |
| `style`   | `text`             | `text`: the body as plain message text. `markdown`: the event's sections as bold headings with their lines below, rendered by the ntfy web app (mobile apps show the markdown source). |
| `routing` | -                  | The per-channel routing block, applied by tuliprox before the plugin is called.                      |

## What ntfy receives

The plugin uses ntfy's JSON publishing: `POST {url}` with `topic`, `title`, `message`, `priority` and `tags`. Because the title is
not an HTTP header, it can carry any Unicode text, emoji included.

- **Priority:** 2 for info, 3 for a warning, 4 for an error (a critical event arrives at a plugin as an error).
- **Tags:** the event's domain (`playlist`, `recording`, ...), plus `warning` or `rotating_light` for a warning or an error, which ntfy shows as emoji.
- **Size:** the message is cut at ntfy's default limit of 4096 **bytes**, on a character boundary, ending in `…`. A larger message
  would become an attachment, or be refused by a server without an attachment cache.
- **Token:** sent only as `Authorization: Bearer ...`, never in a URL.

## Delivery

| ntfy answers                             | Result                                                |
|------------------------------------------|-------------------------------------------------------|
| 2xx                                      | delivered                                             |
| 429, 408                                 | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                   | retried with backoff                                  |
| 501, 505, 3xx and every other 4xx (a wrong token is a 401 or 403) | dead-lettered           |
| no answer within 25 s                    | retried with backoff                                  |
| `url` or `topic` missing, `url` not http(s) | dead-lettered, nothing is sent                     |

The token never appears in a log line or an error message.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-ntfy-notifier` writes `target/plugin-ntfy-notifier/ntfy-notifier.tlpx`, which can be uploaded in the Plugin
Manager.
