# Telegram Notifier

`telegram-notifier` is a notification plugin that sends tuliprox notifications to Telegram chats through a bot. It is a
regular plugin: install and approve it in the Plugin Manager, then configure it under `plugins.plugins`.

```yaml
plugins:
  enabled: true
  plugins:
    - id: telegram-notifier
      enabled: true
      settings:
        bot_token: "${env:TELEGRAM_BOT_TOKEN}"   # from @BotFather; a credential
        chat_ids:
          - "123456789"             # a user or a group
          - "-1001234567890:42"     # a topic (thread) of a forum group
          - "@my_channel"           # a public channel
        style: html                 # html (default) or text
        routing:
          min_severity: warn
          quiet_hours: "23:00-07:00"
```

The channel appears as `plugin.telegram-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying
outbox apply exactly as for the built-in channels (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting     | Default | Description                                                                                                                 |
| ----------- | ------- | --------------------------------------------------------------------------------------------------------------------------- |
| `bot_token` | -       | The bot's API token (`<id>:<secret>`). Write-only in the UI; use `${env:VAR}` to keep it out of the file.                   |
| `chat_ids`  | -       | The chats to send to: `<chat_id>`, `<chat_id>:<thread_id>` or `@channel`. The bot must be a member (or admin of a channel). |
| `style`     | `html`  | `html`: the title and the section headings in bold. `text`: plain text.                                                     |
| `routing`   | -       | The per-channel routing block, applied by tuliprox before the plugin is called.                                             |

Compared with the built-in `messaging.telegram` channel: `bot_token` and `chat_ids` keep their meaning and format. `markdown: true` is replaced by
`style: html`; the plugin does not use MarkdownV2 and does not read `messaging_templates/telegram_*.templ`.

## What Telegram receives

One `sendMessage` per chat and part, `POST https://api.telegram.org/bot<token>/sendMessage`, with `chat_id`, `text`,
`parse_mode: HTML` (in the `html` style) and `message_thread_id` for a topic.

* The first line is the event title, with `⚠️` for a warning and `🚨` for an error (a critical event arrives as an error).
* Then the event's sections, each a bold heading and its lines, separated by a blank line; an event without sections shows
  its body.
* In `html` only `&`, `<` and `>` are escaped, so a playlist called `a<b>` is shown as written.
* A message is split between lines into parts of at most 4000 UTF-16 units (Telegram's limit is 4096). At most 8 parts are
  sent; the last one ends in `…` when something was left out. A single line longer than a part is cut at a character.
* HTML that Telegram still refuses (`can't parse entities`) is sent again, for that part, as plain text.

## Delivery

A *unit* is one part for one chat. The plugin remembers the delivered units in its key-value store (for at most three days,
and at most 100 notifications at once), so a retry sends only what is missing.

| Telegram answers                                                                    | Result                                                                  |
| ----------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| 2xx                                                                                 | the unit is delivered                                                   |
| 429                                                                                 | retried after Telegram's `retry_after` (or `Retry-After`), at most 24 h |
| 5xx except 501 and 505, no answer within 10 s                                       | retried with backoff                                                    |
| 400 (chat not found), 403 (bot blocked or removed), 401 (wrong token) and other 4xx | that chat is given up on; its remaining parts are not sent              |

The notification is delivered when every chat that can take it has it. If one chat is refused permanently and another has the
message, the notification counts as delivered (the log says how many chats were refused). If every chat refuses it, it is
dead-lettered. When several chats need a retry, the longest `retry_after` wins. One attempt stops sending after 20 seconds and
leaves the rest to a retry.

A missing or malformed `bot_token` or `chat_ids` entry is dead-lettered without sending anything; the message names the entry by
its position, never by its text.

The bot token never appears in a log line or an error message, although it is part of every request URL: errors are built from a
status, Telegram's own description or a fixed phrase, never from a URL.

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-telegram-notifier` writes `target/plugin-telegram-notifier/telegram-notifier.tlpx`, which can be uploaded in the
Plugin Manager.
