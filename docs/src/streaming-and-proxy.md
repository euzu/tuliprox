# Streaming And Proxy

Tuliprox is not only a playlist transformer.
Its runtime streaming behavior is a major part of the project.

## Reverse proxy vs redirect

Tuliprox can either redirect to provider URLs or proxy traffic itself.
Proxy mode gives Tuliprox control over:

- user limits
- provider limits
- custom fallback responses
- HLS and catchup session handling
- stream sharing

## `reverse_proxy.stream`

Important fields:

- `retry`
- `buffer`
- `throttle`
- `grace_period_millis`
- `grace_period_timeout_secs`
- `grace_period_hold_stream`
- `shared_burst_buffer_mb`
- `hls_session_ttl_secs`
- `catchup_session_ttl_secs`

### `retry`

If `true`, Tuliprox retries provider streams when the upstream disconnects unexpectedly.

### `buffer`

`buffer` has:

- `enabled`
- `size`

`size` is the number of 8192-byte chunks.
`1024` is roughly `8 MB`.

When `share_live_streams` is enabled, each shared live channel keeps at least the shared burst buffer in memory.

### `throttle`

Bandwidth throttling supports units such as:

- `KB/s`
- `MB/s`
- `KiB/s`
- `MiB/s`
- `kbps`
- `mbps`
- `Mibps`

### Grace period

`grace_period_millis` and `grace_period_timeout_secs` protect users during rapid reconnects, seeks and channel switches.

`grace_period_hold_stream` decides whether Tuliprox waits for the grace decision before it starts sending media data.

## HLS and catchup session reservation

HLS and catchup behave differently from plain TS streaming because clients repeatedly connect, fetch data, disconnect and reconnect.
Tuliprox therefore keeps a short-lived provider-account reservation rather than holding a real provider slot open the entire time.

### `hls_session_ttl_secs`

For HLS:

- the real provider slot is held only during the active playlist or segment request
- between requests, Tuliprox keeps only an account reservation
- the same client/session tries to reuse the same provider account
- channel switches from the same client can take over the reservation immediately

### `catchup_session_ttl_secs`

Catchup has the same account-affinity problem, especially during seeks and reconnects.
Tuliprox therefore applies the same reservation model to catchup.

Important distinction:

- normal TS streaming does not use this family reservation model
- HLS and catchup do

## Shared live streams

When enabled, multiple users can attach to the same upstream live stream instead of opening separate provider connections.
That reduces provider pressure but also means stream priority has to be managed at the shared-stream level.

Current behavior:

- the first viewer starts a shared stream immediately
- additional viewers on the same channel join the existing shared stream
- the effective priority of the shared stream is always the highest priority of its remaining viewers
- when a viewer leaves, the shared stream priority is recalculated
- if provider capacity is full and a higher-priority user starts another stream, the lower-priority shared stream can be preempted
- equal priority does not preempt a different running stream

## Priority and preemption

Tuliprox can prioritize users and internal tasks differently.
Higher-priority user streams can displace lower-priority traffic when provider capacity is exhausted.

The important design rule is:

- user playback wins over low-priority internal work

Priority uses a nice-style scale:

- lower number = higher priority
- negative values are allowed
- equal priority does not preempt a different stream

Probe tasks use `metadata_update.probe.user_priority`.
If preempted by a higher-priority user, they are cancelled immediately and release provider capacity right away.

## Custom stream responses

When Tuliprox cannot serve the real stream, it can return custom fallback videos for cases such as:

- user connection exhausted
- provider connection exhausted
- channel unavailable
- low-priority stream preempted

That makes failure modes easier to understand for end users and downstream clients.

The fallback files are discovered by filename in `custom_stream_response_path`.

## Other reverse-proxy sections

### `cache`

LRU cache for proxied resources such as logos.
If `resource_rewrite_disabled` is set to `true`, the cache is effectively disabled because Tuliprox can no longer rewrite and track resource URLs safely.

### `resource_rewrite_disabled`

Disable rewritten resource URLs when Tuliprox runs behind another proxy and you do not want Tuliprox-generated asset URLs.

### `rate_limit`

Per-IP rate limiting with:

- `enabled`
- `period_millis`
- `burst_size`

### `disabled_header`

Controls which request headers are stripped before upstream requests:

- `referer_header`
- `x_header`
- `cloudflare_header`
- `custom_header`

### `resource_retry`

Controls retries for proxied upstream resources:

- `max_attempts`
- `backoff_millis`
- `backoff_multiplier`

### `geoip`

Optional country lookup from CSV IP ranges.

### `rewrite_secret`

Persistent secret used for generating and validating rewritten resource URLs.
Set it explicitly if rewritten URLs should survive restarts unchanged.

## VLC seek problem and grace tuning

Seeking often produces very fast reconnects and partial range requests.
If the previous upstream connection is not fully closed yet, the provider can still count it against max-connections.

The usual mitigation is:

```yaml
reverse_proxy:
  stream:
    grace_period_millis: 2000
    grace_period_timeout_secs: 5
```

That allows a short overlap, then re-checks whether stale connections have disappeared before enforcing the limit.
