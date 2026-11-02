# Discord Notifier

`discord-notifier` is a notification plugin that posts tuliprox notifications to a Discord webhook. It is a regular
plugin: install and approve it in the Plugin Manager, then configure it under `plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: discord-notifier
      enabled: true
      settings:
        url: "${env:DISCORD_WEBHOOK}"   # the webhook URL is a credential
        style: embed                     # embed (default) or text
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.discord-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the
retrying outbox apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting   | Default | Description                                                                                              |
|-----------|---------|----------------------------------------------------------------------------------------------------------|
| `url`     | -       | The Discord webhook URL. Write-only in the UI; use `${env:VAR}` to keep it out of the file.              |
| `style`   | `embed` | `embed`: one embed with the title, the body, a colour per severity and the instance name as footer. `text`: the body as plain message content. |
| `routing` | -       | The per-channel routing block, applied by tuliprox before the plugin is called.                          |

Titles are cut at 256 characters, embed descriptions at 4096 and plain content at 2000 (by character). Mentions are
disabled, so event text cannot ping `@everyone` or a role.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them. To add a language, add the file
and list it under `ui.locales` in the manifest (see [Settings](./settings.md#localisation-of-the-form)).

## Delivery

| Discord answers                          | Result                                                |
|------------------------------------------|-------------------------------------------------------|
| 2xx                                      | delivered                                             |
| 429, 408                                 | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                   | retried with backoff                                  |
| 501, 505, 3xx and every other 4xx        | dead-lettered                                         |
| no answer within 25 s                    | retried with backoff                                  |

The webhook URL never appears in a log line or an error message.

## Building

`make plugin-discord-notifier` writes `target/plugin-discord-notifier/discord-notifier.tlpx`, which can be uploaded in
the Plugin Manager.
