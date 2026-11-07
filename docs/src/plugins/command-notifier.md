# Command Notifier

`command-notifier` is a notification plugin that runs a local program for each tuliprox notification, with the event as
JSON on its standard input. It is for everything that has no notifier plugin of its own: a script that sends mail, writes a
log, drives home automation.

**It needs the `process` capability**, the one capability that leaves the sandbox: the program runs on the server, as the
user tuliprox runs as. The review in the Plugin Manager says so, and only an admin can approve it. See
[Running programs](./security.md#running-programs).

```yaml
plugins:
  enabled: true
  plugins:
    - id: command-notifier
      enabled: true
      settings:
        program: "/config/scripts/notify.sh"
        args: [ "--from-tuliprox" ]
        timeout_secs: 20
        routing:
          min_severity: warn
```

The channel appears as `plugin.command-notifier`. Routing, quiet hours, suppression, the hourly ceiling and the retrying
outbox apply as for every notifier plugin (see *Per-Channel Routing* in the configuration reference).

## Settings

| Setting        | Default | Description                                                                                                  |
| -------------- | ------- | ------------------------------------------------------------------------------------------------------------ |
| `program`      | -       | The program to run, as a full path. Started directly, never through a shell.                                 |
| `args`         | -       | One entry per argument, passed as written. Do not put a credential here: arguments show in the process list. |
| `timeout_secs` | `20`    | 1 to 25. The program is stopped after this long and the notification is retried.                             |
| `routing`      | -       | The per-channel routing block, applied by tuliprox before the plugin is called.                              |

Compared with the built-in `messaging.command` channel: `program`, `args` and `timeout_secs` keep their meaning
(`timeout_secs` is now at most 25, the default 20, because a plugin call has a 30 second limit). Two things differ:

* **No inherited environment.** The program gets `PATH`, the three variables below, and nothing else, so it no longer sees
  tuliprox's own environment (`HOME`, resolved secrets, ...). A script that needs more sets it itself or reads its own file.
* **No templates.** The program always gets the event as JSON; `messaging_templates/command_*.templ` files are ignored.

## What the program gets

* The event as JSON on **standard input** (`id`, `severity`, `timestamp`, `instance`, `title`, `body`, `fields`,
  `sections`), closed after it is written.
* `TULIPROX_EVENT_ID`, `TULIPROX_EVENT_SEVERITY` (the event's own, `critical` included) and `TULIPROX_EVENT_TITLE` in its
  environment, for a script that only wants to branch on the event.

Its standard output is discarded. Its first line of standard error (at most 160 characters) becomes part of the reason when
it fails.

## Delivery

| The program                                        | Result                                          |
| -------------------------------------------------- | ----------------------------------------------- |
| exits 0                                            | delivered                                       |
| exits with another status, or is ended by a signal | retried with backoff                            |
| runs past `timeout_secs`                           | stopped, retried with backoff                   |
| cannot be started (missing, not executable)        | dead-lettered: it fails the same way every time |
| `program` missing, or `process` not approved       | dead-lettered, nothing is run                   |

## Languages

The settings form is translated into English, Russian, Spanish and Arabic and follows the Web UI language. The texts are
in `ui/locales/<lang>.json`; the schema's titles and descriptions are keys into them.

## Building

`make plugin-command-notifier` writes `target/plugin-command-notifier/command-notifier.tlpx`, which can be uploaded in the
Plugin Manager.
