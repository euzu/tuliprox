# Slack Notifier

`slack-notifier` is a notification plugin that posts tuliprox notifications to a Slack incoming webhook. It is a regular
plugin: install and approve it in the Plugin Manager, then configure it under `plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: slack-notifier
      enabled: true
      settings:
        url: "${env:SLACK_WEBHOOK}"   # the webhook URL is a credential
        style: blocks                  # blocks (default) or text
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.slack-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying
outbox apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting   | Default  | Description                                                                                              |
|-----------|----------|----------------------------------------------------------------------------------------------------------|
| `url`     | -        | The Slack incoming webhook URL. Write-only in the UI; use `${env:VAR}` to keep it out of the file.       |
| `style`   | `blocks` | `blocks`: a header with the title, one block per section (or the body) and a footer with the event id, the instance and the time. `text`: the body as plain message text. |
| `routing` | -        | The per-channel routing block, applied by tuliprox before the plugin is called.                          |

A warning or an error gets a `[warn]` or `[error]` marker in front of the header, because Slack has no severity field.

## What Slack receives

Text is Slack `mrkdwn`. `&`, `<` and `>` are written as entities, so event text can never become a mention such as
`<!channel>` or a link. Headers are cut at 150 characters, sections and plain text at 3000 (by character), and a message
has at most 50 blocks. What does not fit is named in a last block.

## Delivery

| Slack answers                            | Result                                                |
|------------------------------------------|-------------------------------------------------------|
| 2xx                                      | delivered                                             |
| 429, 408                                 | retried after `Retry-After` (seconds or HTTP date, at most 24 h) |
| 5xx except 501 and 505                   | retried with backoff                                  |
| 501, 505, 3xx and every other 4xx        | dead-lettered                                         |
| no answer within 25 s                    | retried with backoff                                  |

The webhook URL never appears in a log line or an error message.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-slack-notifier` writes `target/plugin-slack-notifier/slack-notifier.tlpx`, which can be uploaded in the
Plugin Manager.
