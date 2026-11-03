# Gotify Notifier

`gotify-notifier` is a notification plugin that pushes tuliprox notifications to a [Gotify](https://gotify.net) server. It is
a regular plugin: install and approve it in the Plugin Manager, then configure it under `plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: gotify-notifier
      enabled: true
      settings:
        url: "https://gotify.example.org"
        token: "${env:GOTIFY_TOKEN}"   # the application token is a credential
        style: text                    # text (default) or markdown
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.gotify-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying
outbox apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting   | Default | Description                                                                                              |
|-----------|---------|----------------------------------------------------------------------------------------------------------|
| `url`     | -       | The base URL of the Gotify server, without `/message`.                                                   |
| `token`   | -       | The token of a Gotify application. Write-only in the UI; use `${env:VAR}` to keep it out of the file.    |
| `style`   | `text`  | `text`: the body as plain message text. `markdown`: the event's sections as bold headings with their lines below, rendered by Gotify clients (the plain body when an event has none). |
| `routing` | -       | The per-channel routing block, applied by tuliprox before the plugin is called.                          |

## What Gotify receives

`POST {url}/message` with `title`, `message` and `priority`: 2 for info, 5 for a warning, 7 for an error (a critical event
arrives at a plugin as an error). The token is sent in the `X-Gotify-Key` header, not as a `?token=` query parameter, so it
never appears in a URL. With `style: markdown` the message carries the `client::display` extra that makes clients render
it, and markdown characters in event text are escaped, so a playlist called `my_list*` shows as written.

## Delivery

| Gotify answers                           | Result                                                |
|------------------------------------------|-------------------------------------------------------|
| 2xx                                      | delivered                                             |
| 429, 408                                 | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                   | retried with backoff                                  |
| 501, 505, 3xx and every other 4xx (a wrong token is a 401) | dead-lettered                       |
| no answer within 25 s                    | retried with backoff                                  |
| `url` or `token` missing, `url` not http(s) | dead-lettered, nothing is sent                     |

The token never appears in a log line or an error message.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-gotify-notifier` writes `target/plugin-gotify-notifier/gotify-notifier.tlpx`, which can be uploaded in the
Plugin Manager.
