# Shared HLS Troubleshooting

This page helps operators diagnose Shared HLS problems without reading the source code.

Start with the checklist, then use the symptom sections below.

## Quick checklist

1. Confirm `reverse_proxy.hls_cache` exists in `config.yml`.
2. Confirm the target has `options.share_live_streams.hls: true` in `source.yml`.
3. Confirm the HLS cache path exists or can be created by the Tuliprox process.
4. Confirm `reverse_proxy.rewrite_secret` is stable and has not changed between restarts.
5. Confirm the client follows HTTP `307 Temporary Redirect` responses.
6. Confirm the client is opening the generated Tuliprox HLS URL, not a stale `/hls/shared/live/...` URL from a previous playback.
7. Check logs for `HLS access lease rejected`, `HLS session created`, `HLS session reused`, and `HLS access leases marked channel unavailable`.

## How to recognize Shared HLS in URLs

Shared HLS URLs contain `/hls/shared/live/`:

```text
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/manifest.m3u8
```

The first token identifies the shared session. The second token is the per-playback access lease.

A healthy first request usually looks like this:

```text
GET /hls/...original generated live URL...
307 Temporary Redirect
Location: /hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/manifest.m3u8
```

## Symptom: the player never reaches `/hls/shared/live/`

Likely causes:

| Cause | Check | Fix |
| :--- | :--- | :--- |
| Global cache block missing | Search `config.yml` for `reverse_proxy.hls_cache`. | Add the `hls_cache` block. |
| Target switch missing | Search the target in `source.yml`. | Add `options.share_live_streams.hls: true`. |
| Client uses direct source URL | Look at the URL in the player or access log. | Use the generated Tuliprox playlist URL. |
| Client does not follow redirects | Look for the initial `307` without a follow-up request. | Use a client that supports redirects, or fix reverse proxy/client behavior. |

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

This means Tuliprox accepted the user request, but the shared HLS session could not currently produce a usable channel
response.

Possible causes:

| Area | What to look for |
| :--- | :--- |
| Manifest fetch | `HLS manifest temporary failure counted` or `HLS manifest temporary failure threshold reached`. |
| Manifest commit | `HLS access lease marked channel unavailable after fresh manifest commit failed`. |
| Segment fetch | `HLS segment temporary failure counted`, `HLS segment temporary failure threshold reached`, or permanent segment failures. |
| MAP/transient resource fetch | MAP or transient object failure messages. |
| Provider account | Origin account unavailable or provider lineup exhaustion. |

Fix:

1. Test the origin HLS URL from the Tuliprox host.
2. Check provider account limits and provider availability.
3. If the provider produces unstable manifests, try `manifest_recovery_burst.level: "friendly"` before using stronger levels.
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
| `HLS access leases marked channel unavailable` | Tuliprox gave up for the current lease/session after repeated failures. |

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
| `HLS access lease prepared` | Entry request created a new access lease and returned the shared HLS redirect. |
| `HLS access lease accepted` | Manifest or media request validated the lease. |
| `HLS access lease rejected` | Lease validation or admission failed. Check the reason suffix. |
| `HLS access lease idled` | Lease left active media state and released its active stream reservation. |
| `HLS access lease removed` | Lifecycle cleanup removed the lease. |
| `HLS session created` | A new shared session was created for a session key. |
| `HLS session reused` | Existing shared session was reused for the same session key. |
| `HLS session lifecycle expired` | Idle shared session was cleaned up. |
| `HLS lifecycle state snapshot` | Debug summary of sessions, leases, QoS, repair, and cleanup state. |
| `HLS manifest rendered` | A shared manifest was successfully rendered for a player. |
| `HLS segment cached` | A segment was fetched and committed to the cache. |
| `HLS session switched to transient passthrough` | The manifest required transient resource handling. |

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
reuse, missing redirect support, aggressive caching, or reconnect behavior in the client.
