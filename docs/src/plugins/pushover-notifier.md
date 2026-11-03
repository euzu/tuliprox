# Pushover Notifier

`pushover-notifier` is a notification plugin that sends tuliprox notifications to [Pushover](https://pushover.net). It is a
regular plugin: install and approve it in the Plugin Manager, then configure it under `plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: pushover-notifier
      enabled: true
      settings:
        token: "${env:PUSHOVER_TOKEN}"   # the API token of your Pushover application
        user: "${env:PUSHOVER_USER}"     # the user (or group) key that receives the messages
        style: text                      # text (default) or html
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.pushover-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying
outbox apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting   | Default                                        | Description                                                                                     |
|-----------|------------------------------------------------|-------------------------------------------------------------------------------------------------|
| `token`   | -                                              | The API token of your Pushover application. Write-only in the UI; use `${env:VAR}` to keep it out of the file. |
| `user`    | -                                              | The user (or group) key that receives the notifications. Write-only in the UI; use `${env:VAR}`. |
| `url`     | `https://api.pushover.net/1/messages.json`     | The messages endpoint. Leave the default unless you use a compatible server.                    |
| `style`   | `text`                                         | `text`: the body as plain message text. `html`: the event's sections as bold headings with their lines below, shown formatted by the Pushover apps. |
| `routing` | -                                              | The per-channel routing block, applied by tuliprox before the plugin is called.                 |

## What Pushover receives

An `application/x-www-form-urlencoded` POST with `token`, `user`, `title`, `message`, `priority` and `timestamp` (and `html=1` in
the `html` style). The credentials are in the body, never in the URL.

- **Priority:** -1 (quiet) for info, 0 for a warning, 1 (high priority) for an error. A critical event arrives at a plugin as an error.
- **Time:** `timestamp` is the event's own time, so a message that reached Pushover late through the outbox still shows when it happened.
- **Limits:** a title is cut at 250 characters and a message at 1024 (by character), a cut ending in `…`. In the `html` style the
  message is assembled from whole lines, so the cut never splits a tag or an entity such as `&amp;`; a text line that does
  not fit is cut at an entity boundary. `&`, `<` and `>` are written as entities.

## Delivery

| Pushover answers                         | Result                                                |
|------------------------------------------|-------------------------------------------------------|
| 2xx                                      | delivered                                             |
| 429, 408                                 | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                   | retried with backoff                                  |
| 501, 505, 3xx and every other 4xx (a wrong token or user key is a 400) | dead-lettered |
| no answer within 25 s                    | retried with backoff                                  |
| `token` or `user` missing, `url` not http(s) | dead-lettered, nothing is sent                    |

The token and the user key never appear in a log line or an error message. For that reason the reason Pushover gives for a
refusal (its `errors` list) is not copied into the log either: a 400 means check the token and the user key.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-pushover-notifier` writes `target/plugin-pushover-notifier/pushover-notifier.tlpx`, which can be uploaded in the
Plugin Manager.
