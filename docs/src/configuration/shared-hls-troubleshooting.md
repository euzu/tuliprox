# Shared HLS Troubleshooting

This page helps operators diagnose Shared HLS problems without reading the source code.

Start with the checklist, then use the symptom sections below.

## Quick checklist

1. Confirm `reverse_proxy.hls_cache` exists in `config.yml`.
2. Confirm the target has `options.share_live_streams.hls: true` in `source.yml`.
3. Confirm the HLS cache path exists or can be created by the Tuliprox process.
4. Confirm `reverse_proxy.rewrite_secret` is stable and has not changed between restarts.
5. Confirm the client loads the single variant from the initial HTTP `200` master playlist.
6. Confirm the client is opening the generated Tuliprox HLS URL, not a stale `/hls/shared/live/...` URL from a previous playback.
7. Check logs for `HLS manifest acceptance full burst started`, terminal bundle preparation, critical handoff, and terminal commit retry outcomes.

## How to recognize Shared HLS in URLs

Shared HLS URLs contain `/hls/shared/live/`:

```text
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/manifest.m3u8
```

The first token identifies the shared session. The second token is the per-playback access lease.

A healthy first request usually looks like this:

```text
GET /hls/...original generated live URL...
200 OK
Content-Type: application/vnd.apple.mpegurl

#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=<advertised bitrate>
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/manifest.m3u8
```

The player then reloads the lease-bound media-playlist URI, not the generated entry URL. Unknown streams advertise
`BANDWIDTH=1000000`; a known or previously learned bitrate is advertised with safety headroom.

## Symptom: the player never reaches `/hls/shared/live/`

Likely causes:

| Cause | Check | Fix |
| :--- | :--- | :--- |
| Global cache block missing | Search `config.yml` for `reverse_proxy.hls_cache`. | Add the `hls_cache` block. |
| Target switch missing | Search the target in `source.yml`. | Add `options.share_live_streams.hls: true`. |
| Client uses direct source URL | Look at the URL in the player or access log. | Use the generated Tuliprox playlist URL. |
| Client does not load the master variant | Look for the initial `200` without a follow-up request to its `/hls/shared/live/.../manifest.m3u8` variant. | Use a client that supports HLS master playlists, or fix reverse proxy/client behavior. |

## Symptom: `404 Not Found` on `/hls/shared/live/...`

A `404` usually means the URL does not match live server-side state.

Common reasons:

- the player reused an old manifest, segment, MAP, or resource URL after the lease expired;
- Tuliprox restarted and in-memory leases were lost;
- the HLS cache path changed and Tuliprox cleared runtime HLS state;
- `rewrite_secret` changed and future public session IDs no longer match old URLs;
- the URL was copied from one user or client to another.

Fix:

1. Make the player reopen the original generated Tuliprox playlist entry URL.
2. Check whether Tuliprox restarted or reloaded a changed HLS cache path.
3. Keep `rewrite_secret` stable.
4. Do not share canonical `/hls/shared/live/...` URLs between users.

## Symptom: custom video `hls_session_or_lease_expired`

This means the player requested a Shared HLS path whose access lease or shared session is no longer usable.

Common causes:

- long pause or device sleep;
- IPTV app resumed with old segment URLs instead of reopening the playlist;
- `session_idle_timeout` is too low for the client’s pause/reconnect behavior;
- Tuliprox was restarted while the player kept old URLs.

Fix:

- Ask the client to reload the channel from the playlist.
- Keep `session_idle_timeout` at the default `300` seconds unless you have a reason to lower it.
- Increase `session_idle_timeout` only if stale-resume behavior is common and you can afford longer runtime retention.

## Symptom: custom video `channel_unavailable`

For the warm Shared HLS path, this is a lease-local terminal decision rather than a failure-counter threshold. The
canonical manifest stays on the same URL and returns an HTTP `200` finite playlist when a compatible terminal tail is
ready. An incompatible MAP/key/container/track state fails closed with `503`; it never creates an unsafe TS splice.

Possible causes:

| Area | What to look for |
| :--- | :--- |
| Origin progress | Repeated valid HTTP `200` manifests without host-local media progress, or retryable/hard fetch evidence. |
| Manifest acceptance | `HLS manifest acceptance full burst started`, its derived candidate count, and whether a pinned or staged cross-host candidate commits. |
| Lease reserve | The affected lease reaches its recovery/transition boundary before progress commits. |
| Terminal preparation | `HLS terminal bundle preparation unavailable` with a typed failed or incompatible reason. |
| Terminal commit | `HLS autonomous terminal owner failed closed` or `HLS terminal commit retry owner failed closed`. |
| Segment/MAP/transient media | Required READY reserve cannot be established, or a required object remains unavailable. |
| MAP/transient resource fetch | MAP or transient object failure messages. |
| Provider account | Origin account unavailable or provider lineup exhaustion. |

Fix:

1. Test the origin HLS URL from the Tuliprox host.
2. Check provider account limits and provider availability.
3. Verify that the configured recovery burst has enough upstream capacity. Its first episode attempt always executes the
   complete configured plan; `beast` derives six slots by two lanes, not a reduced pre-probe.
4. If TS segments are corrupt, try `segment_repair.max_level: "low"` only after confirming segment errors in logs.

## Symptom: custom video `user_connections_exhausted`

Tuliprox rejected the playback because the Tuliprox user has no available stream capacity.

Shared HLS does not bypass user limits. Each viewer still needs an accepted Tuliprox user session and access lease.

Fix:

- Check the user’s configured connection limit.
- Check whether old clients are keeping sessions alive.
- Review active streams in the Tuliprox UI/API if available.

## Symptom: custom video `provider_connections_exhausted`

Tuliprox could not acquire a provider account or provider connection for origin work.

Shared HLS reduces duplicate origin work after a session exists, but the first session for a channel still needs origin
access.

Fix:

- Check provider account connection limits.
- Check whether other live, catchup, or non-HLS sessions are using the same provider accounts.
- Review priority and preemption settings.
- Confirm that the target really shares HLS. If not, multiple viewers may be opening separate legacy HLS paths.

## Symptom: low-priority stream is interrupted

Low-priority or soft connections can be preempted according to the normal connection-handling policy.

Fix:

- Review user and target priority settings.
- See [Priorities, Soft Connections and Preemption](./connection-handling-priorities-and-preemption.md).
- Check for `low_priority_preempted` custom-video redirects.

## Symptom: repeated `503 Service Unavailable` with `Retry-After`

A `503` with `Retry-After` can be normal during short origin/cache waits, especially near startup or when a requested
segment is known but not ready yet.

Investigate when it repeats for too long:

| Log signal | Interpretation |
| :--- | :--- |
| `HLS segment demand fetch skipped by backpressure` | The session or global fetch limit is saturated. |
| `HLS segment temporary failure counted` | The origin segment fetch is failing temporarily. |
| `HLS manifest marked fresh-commit required after hard fetch failure` | Manifest refresh had a hard failure and needs a fresh commit. |
| `HLS terminal manifest failed closed` | A typed lease-local terminal decision could not safely serve live or terminal media. The reason identifies state, bundle, deadline, capacity, or retry failure. |
| `HLS terminal commit retry owner failed closed` | The bounded autonomous commit worker reached a typed terminal failure; inspect the reason and preceding recovery/bundle events. |

Fix:

- Keep default concurrency until the real bottleneck is known.
- Increase `max_concurrent_segment_fetches_global` only when CPU, disk, and provider bandwidth can handle it.
- Avoid setting per-session concurrency too high; one problematic channel can otherwise dominate origin work.

## Symptom: cache directory grows too large

Check these settings:

| Setting | Effect |
| :--- | :--- |
| `cache_bytes` | Global HLS cache budget. |
| `cache_bytes_per_session` | Per-session budget. |
| `cache_duration` | Retention baseline for unprotected objects. |
| `session_idle_timeout` | Longer timeouts keep sessions and lease-related state around longer. |

Fix:

- Lower `cache_bytes_per_session` if single channels grow too large.
- Lower `cache_duration` if unprotected old objects stay too long.
- Put `cache_path` on a disk with enough free space and predictable cleanup behavior.

## Symptom: cache path permission errors

The Tuliprox process must be able to create directories, write temporary files, rename committed cache objects, read cached
objects, and delete expired files.

Fix:

```bash
mkdir -p /var/lib/tuliprox/cache/hls
chown -R tuliprox:tuliprox /var/lib/tuliprox/cache/hls
chmod 750 /var/lib/tuliprox/cache/hls
```

Adapt the user and group to your deployment.

## Useful log messages

| Message fragment | Meaning |
| :--- | :--- |
| `HLS access lease prepared` | Entry request created a new access lease and returned its single-variant master playlist. |
| `HLS access lease accepted` | Manifest or media request validated the lease. |
| `HLS access lease rejected` | Lease validation or admission failed. Check the reason suffix. |
| `HLS access lease idled` | Lease left active media state and released its active stream reservation. |
| `HLS access lease removed` | Lifecycle cleanup removed the lease. |
| `HLS session created` | A new shared session was created for a session key. |
| `HLS session reused` | Existing shared session was reused for the same session key. |
| `HLS session lifecycle expired` | Idle shared session was cleaned up. |
| `HLS lifecycle state snapshot` | Debug summary of sessions, leases, QoS, repair, and cleanup state. |
| `HLS manifest rendered` | A shared manifest was successfully rendered for a player. |
| `HLS manifest acceptance full burst started` | A generation-bound episode started the full configured candidate plan. The logged count is derived from configuration. |
| `HLS manifest acceptance landscape changed` | Candidate evidence changed and the runtime started full requalification under a new generation. |
| `HLS critical manifest handoff verified` | The endangered lease's base and staged cross-host candidate passed bounded track/compatibility checks. |
| `HLS terminal bundle preparation unavailable` | Prepared media is failed or incompatible; no HTTP request attempts an on-demand TS rewrite. |
| `HLS autonomous terminal owner handed off` | The endpoint handed terminal publication to the bounded autonomous commit retry owner. |
| `HLS terminal commit retry owner completed` | A generation-bound retry reached a terminal completion outcome. |
| `HLS terminal commit retry owner failed closed` | Retry exhausted, missed its safe deadline, lost runtime capacity, or observed incompatible media. |
| `HLS segment cached` | A segment was fetched and committed to the cache. |
| `HLS session switched to transient passthrough` | The manifest required transient resource handling. |

## Metrics, bounds, and alerting

The existing in-memory HLS metrics expose session/lease counts, refresh started/completed/retried/failed, manifest render
results, cache/range hits, demand/prefetch work, cached/removed objects, GC runs, and secret-marker events. Use ratios and
rates rather than a single request count: for example, compare `refresh_failed` with `refresh_started`, and correlate
manifest-render gaps with active leases and ready-media access.

Recovery state is intentionally bounded:

- TS inspection reads only its byte/packet/resynchronization probe budget;
- acceptance candidate lists, samples, cohorts, and observed-latency histories have fixed caps;
- prepared bundles use bounded entry, byte, and in-flight limits;
- terminal pending owners and commit retry owners have capacity, attempt, deadline, and worker-restart limits;
- GC protection lasts only while a live terminal lease references its base suffix.

Configure log-derived alerts with local traffic-aware windows:

| Alert | Trigger evidence | Operator response |
| :--- | :--- | :--- |
| `ProbeBudgetExhausted` | Repeated TS track evidence reason `probe-budget-exhausted`, especially on critical handoffs. | Inspect segment framing/size and origin corruption. Do not increase the probe blindly; rejected evidence is fail-closed. |
| `BundlePrepareFailed` | Repeated `HLS terminal bundle preparation unavailable: state=failed` or `state=incompatible`. | Validate the configured `channel_unavailable` TS, target duration, tracks, container, MAP, and key transition. |
| `RecoveryMissedDeadline` | Terminal failed-closed reason `safe_commit_deadline_elapsed`, or terminal cutover follows a full burst with no committed progress. | Check origin latency, candidate availability, READY reserve, and recovery ETA; request counters are not the cause. |
| `TerminalCommitRetry` | Sustained retry handoffs, `retry_attempts_exhausted`, `retry_capacity_exceeded`, worker restart exhaustion, or invalid lock-busy completion. | Inspect lock pressure and task health; retain the bounded retry/deadline settings while diagnosing. |

Do not put origin URLs, credentials, capability tokens, manifest bodies, or raw lease/session identifiers into alert
labels. Tuliprox logs hashed/sanitized identifiers where correlation is necessary.

## Staged rollout and rollback

1. Deploy to a canary with representative TS, encrypted TS, fMP4/MAP, multi-origin, and stale-but-reachable channels.
2. Observe for at least one normal peak window plus the longest expected origin incident. Compare refresh failures,
   acceptance bursts, range hits, bundle failures, missed deadlines, retry handoffs, terminal HTTP status, and player
   reconnect behavior with the pre-rollout baseline.
3. Expand in stages only when full-burst candidate counts, cross-host discontinuities, terminal `200` responses, and
   sticky lease behavior match expectations. Keep cold-start and provisioning failure rates separate from warm recovery.
4. Prefer a forward fix for isolated provider/container incompatibility. The terminal path fails closed, so do not add a
   counter threshold or bypass MAP/key/track validation as an emergency workaround.
5. If systemic regressions require rollback, revert the complete feature release to its previous compatible binary and
   let clients reopen the generated entry URL. Do not mix binaries that disagree on lease/generation-bound terminal URLs
   or selectively restore the old warm `307` behavior.

## Safe first debugging command sequence

Use equivalent commands for your deployment:

```bash
# 1. Confirm config switches
grep -R "hls_cache" config.yml
grep -R "share_live_streams" source.yml

# 2. Confirm cache path permissions
ls -ld /var/lib/tuliprox/cache/hls

# 3. Watch HLS-related logs
grep -i "HLS " tuliprox.log | tail -200
```

If the issue is client-specific, compare a working player and a failing player. Many HLS issues are caused by stale URL
reuse, failure to load the master-playlist variant, aggressive caching, or reconnect behavior in the client.
