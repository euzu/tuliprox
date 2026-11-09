# Matrix Notifier: manual checks (server mode)

Ports: tuliprox `18980`, mock Matrix homeserver `18972`. All commands run from the
repo root; `RUN` is a scratch directory. Nothing here is committed output.

## 0. Set up

```sh
export RUN=/tmp/matrix-notifier-demo
rm -rf $RUN && mkdir -p $RUN/cfg $RUN/plugins/matrix-notifier $RUN/web
D=plugins/examples/matrix-notifier

bin/package-plugin.sh $D                       # builds plugin.wasm, writes target/plugin-examples/matrix-notifier.tlpx
unzip -q target/plugin-examples/matrix-notifier.tlpx -d $RUN/plugins/matrix-notifier

cp $D/demo/config.yml $D/demo/source.yml $D/demo/api-proxy.yml $RUN/cfg/ && cp $D/demo/input.m3u $RUN/
sed -i.bak "s#__RUN__#$RUN#g" $RUN/cfg/*.yml && rm $RUN/cfg/*.bak

python3 -I $D/mock/matrix_hs.py --port 18972 > $RUN/mock.log 2>&1 &     # the homeserver
(cd $RUN && /path/to/tuliprox -s -c cfg/config.yml -i cfg/source.yml -a cfg/api-proxy.yml > server.log 2>&1) &
```

The binary must be a build that has the plugin system. `update_on_boot: true` runs one
refresh at start. Wait for `stats:` in `$RUN/server.log`, then let it go quiet.

Helpers used below:

```sh
HS=http://127.0.0.1:18972; TP=http://127.0.0.1:18980
stats()   { curl -s $HS/_stats | python3 -I -m json.tool; }
plog()    { curl -s "$TP/api/v1/plugins/matrix-notifier/logs?limit=500" | python3 -I -m json.tool; }
refresh() { curl -s -o /dev/null -w '%{http_code} in %{time_total}s\n' -X POST $TP/api/v1/playlist/update \
              -H 'content-type: application/json' -d '{"targets":["demo"]}'; }
```

Sanity first: `GET $TP/api/v1/plugins` lists `matrix-notifier` with state `enabled`. If it is
`invalid` or `pending_approval`, the rest tests nothing (grep the server log for
`is invalid`). If `GET /api/v1/plugins` is a 404, `web_ui.enabled` is missing.

## (a) A refresh reaches the room through the outbox

1. `curl -s -X POST $HS/_reset` (forget the boot message), then `refresh` (expect `202`).
2. Wait about 3 s, then `stats`.

Expected: `count` is `1` (it may be 2 if the boot refresh was still delivering; reset and retry
once it is quiet). `last[0].body` starts with `[INFO] Playlist update finished: 1 input(s), 1 target(s)`, ends with
`-- playlist.update.completed` (plus `@ <instance>` when `messaging` names an instance), and `formatted_body`
starts with `<strong>[INFO]`. `last[0].txn_id` has the form `tuliprox-<32 hex>-<unix seconds>`;
`txn_ids_seen` has exactly that one id with value `1`.

Plugin side (`plog`): one line `sent [INFO]`...`as tuliprox-...` with the same txn id. The
access token appears nowhere (plugin logs redact it, and the plugin never logs it).

Server log: no warning from the plugin. `grep matrix-notifier $RUN/server.log` shows no failure.

## (b) Homeserver down: the outbox retries, nothing stalls, then delivered once

Backoff defaults: first retry after 5 s, doubling (5, 10, 20, 40, 80 s), at most 6 attempts
(`recording.notifications.max_attempts`), then the notification is dead-lettered.

### (b1) Outage, answered with 503

1. `curl -s -X POST $HS/_reset; curl -s -X POST $HS/_mode -d '{"fail":503}'`
2. `refresh`.

Not blocked: the request returns `202` in well under a second (`time_total` of a few ms), and
`GET $TP/api/v1/playlist/update/status` reports the run finished in the same time as in (a).
Compare the run duration in `server.log` with the one from (a); they must match within noise.
The emitter only enqueues; the plugin call happens on the outbox worker.

Retries visible in three places:

- `$RUN/server.log` (debug from `tuliprox_messaging`), one line per attempt:
  `Notification playlist.update.completed to plugin.matrix-notifier will be retried: homeserver answered 503: M_UNKNOWN: homeserver unavailable`
  at about +0 s, +5 s, +15 s, +35 s, +75 s, +155 s.
- `$RUN/work/notification_outbox.json`: one entry with `"pending": ["plugin.matrix-notifier"]`,
  `attempts` rising and `next_attempt_at` moving out by the doubling backoff.
- `plog`: `will retry`...`(tuliprox-<fp>-<T0>): homeserver answered 503 ...`; the txn id is
  IDENTICAL on every line (it keeps the first attempt's timestamp).

`stats` while down: `count` `0`, `attempts` rising, `rejected` `{"503": N}`, and `txn_ids_seen`
holds one id with value N.

1. Bring the homeserver back: `curl -s -X POST $HS/_mode -d '{"fail":null}'`.

Expected at the next retry: `count` `1`, `rejected` stops growing, `plog` shows
`sent [INFO] ... as <the same txn id>`, `notification_outbox.json` no longer holds the entry
(`"entries": []`), and `server.log` shows `Notification ... settled after N attempt(s)`.

### (b2) Lost acknowledgement: the transaction id is what makes it exactly once

1. `curl -s -X POST $HS/_reset; curl -s -X POST $HS/_mode -d '{"fail_after_store":503}'` then `refresh`.
   The mock STORES the message and then answers 503, like a homeserver whose reply was lost.
2. Wait for two retries (about 20 s) and `stats`.

Expected: `count` is `1` already, `attempts` is 3 or more, `rejected` `{"503": N}`, and `txn_ids_seen`
has ONE id with value N (every retry reused it). No second message appears.

1. `curl -s -X POST $HS/_mode -d '{"fail_after_store":null}'` and wait for the next retry.

Expected: that retry answers 200 with the same `event_id`, `count` stays `1`, and the outbox
entry is gone. One notification, one message, despite N attempts.

### (b3) Connection refused

Kill the mock (`kill %1` or the pid), `refresh`, wait for one retry: `plog` shows
`will retry ... http transport error: ...` and the outbox entry stays. Start the mock again
(same command as step 0): the next retry delivers, `count` `1`.

### (b4) Rate limit (429 with `retry_after_ms`)

`curl -s -X POST $HS/_reset; curl -s -X POST $HS/_mode -d '{"rate_limit":1,"retry_after_ms":3000}'`, `refresh`.

Expected: `plog` shows `rate limited, retry`...`(tuliprox-...) in 3s`; the second attempt
comes about 3 s after the first (not the 5 s default backoff: a provider delay wins), and then
`count` is `1` with the same txn id. `rejected` is `{"429": 1}`.

### (b5) Bad token or room is permanent

`curl -s -X POST $HS/_reset; curl -s -X POST $HS/_mode -d '{"rate_limit":0,"fail":null}'`, restart
the mock with `--token other` (or change `ext.messaging.access_token` in the settings), `refresh`.

Expected: the mock answers 401, `plog` shows `refused`...`: homeserver answered 401: M_UNKNOWN_TOKEN: ...`,
`server.log` shows `notification_permanently_rejected` for `plugin.matrix-notifier` ONCE, and the
outbox entry is gone with no retry.

## (c) on-event: one log line per kind, counters in the KV, never a message

`plog` after the steps below must contain, in this shape (values vary):

| Trigger | Line |
| --- | --- |
| a refresh (a) | `event playlist.update state=Success run_id=... stats=N error=false` |
| a client stream (c1) | `event user.connection.changed type=updated user=u uid=...`, later `type=connections users=1 connections=1` and a `type=disconnected...` line |
| a failed probe (c2) | `event stream.probe.failed input=demo-in stream=... reason=NotFound` |

### (c1) user.connection.changed

```sh
curl -s "$TP/get.php?username=u&password=p&type=m3u_plus" | grep -A1 'Mock One'    # the proxied URL
curl -s -o /dev/null <that URL>                                                    # holds ~2 s, then ends
```

Expected: the `user.connection.changed` lines above. Because `notify_on` lists
`user.connection.changed`, each also yields a Matrix message with the footer
`-- user.connection.changed` (that is the outbox delivery, not on-event).

### (c2) stream.probe.failed (needs `ffprobe` on PATH)

The demo input has `probe_live: true`, the config has `metadata_update.ffprobe.enabled: true`, and
the playlist holds `Mock Dead` whose URL the mock answers with 404. Run `refresh`; the probe runs
in the background (a few seconds to minutes).

Expected: the `event stream.probe.failed ... reason=NotFound` line, and a Matrix message
`[WARN] Stream probe failed for input demo-in` (warn is the registry severity).
Without `ffprobe` installed no probe runs and this event cannot be produced here; say so rather
than counting it as a pass.

### (c3) on-event never sends

Stop tuliprox, replace the `messaging:` block of `cfg/config.yml` with
`demo/messaging.events-only.yml`, restart, `curl -s -X POST $HS/_reset`, repeat (c1).

Expected: `plog` has the `event user.connection.changed ...` lines, `stats` shows `count` `0`
(`attempts` `0` as well): the events reached the plugin only through on-event, which logs and
counts but never calls Matrix. A `refresh` then still delivers one message (a).

### Counters

The KV key `event-tally` holds `{"counters": {"<kind>": n, ...}, "last": {"kind", "timestamp", "summary"}}`.
There is no read route for a plugin's KV; the observable is the log line count per kind, and
`GET $TP/api/v1/plugins/matrix-notifier/stats` (call and error counts per hook).

## (d) Which severity floor applies first

The host-side `settings.ext.messaging.routing` runs before the plugin: `routing.min_severity` is compared with
the event's registry severity in the outbox, and a channel that does not want the event is never
called (no plugin log line at all). The plugin's own `min_severity` setting runs inside
`send-notification`, and below it the message is acknowledged without being sent (the outbox counts
it as delivered).

1. `ext.messaging.routing.min_severity: error` (settings PUT, or edit `config.yml` and restart; the form is
   `GET $TP/api/v1/plugins/matrix-notifier/settings`, body `{"settings": {...}}`, the token as
   `"********"` keeps the stored one). `refresh`.
   Expected: no `matrix-notifier` line in `plog`, `stats` `attempts` unchanged. The host skipped it.
2. Remove `ext.messaging.routing.min_severity`, set the plugin `ext.messaging.min_severity: warn`. `refresh`.
   Expected: `plog` has `skipped`...`: INFO is below min_severity Warn`, `stats` `attempts`
   unchanged (nothing sent), the outbox entry is gone. The plugin skipped it.
