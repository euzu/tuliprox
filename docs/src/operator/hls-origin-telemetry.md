# Shared HLS Origin Telemetry

Shared HLS can report how well the **origin** (your provider, possibly behind a VPN) delivers segments, maps, keys and
manifests. It measures the origin fetches Tuliprox already makes. It sends no extra provider requests and keeps no
credentials or full origin URLs. External tools (watchdogs, alerts, VPN exit rotation) can poll it to tell origin
degradation apart from local or downstream bottlenecks. Tuliprox only provides the data; recovery policy stays external.

## 1. Enabling

The feature is **off by default**. Add the block under `reverse_proxy` in `config.yml`:

```yaml
reverse_proxy:
  hls_origin_telemetry:
    enabled: true
    queue_capacity: 1024
    freshness_secs: 60
    window_secs: 300
```

| Parameter        | Type | Default | Meaning                                                                                                                              |
|:-----------------|:----:|:--------|:-------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`        | Bool | `false` | Turns the recorder on. When off, nothing is measured and there is no runtime cost.                                                   |
| `queue_capacity` | Int  | `1024`  | Bounded event queue, at least `16` and at most Tokio's channel limit. When full, events are dropped and counted; fetches never wait. |
| `freshness_secs` | Int  | `60`    | `transfer_quality` values whose newest sample is older than this are reported as `null` (unknown). `availability` is not affected.   |
| `window_secs`    | Int  | `300`   | Rolling window used for the aggregates (at most 4096 samples per input).                                                             |

* The same settings are available in the Web UI under **Config → Reverse Proxy → HLS Origin Telemetry**.
* Unknown keys are rejected. A block equal to the defaults is the same as no block.
* Changing or disabling the block needs a restart. A reload that disables it logs a warning while the recorder keeps
  running until then. Enabling it on a running instance without a recorder takes effect
  on config reload.

## 2. Reading it

`GET /api/v1/hls/origin-telemetry` (permission `system.read`, also available as MCP tool `get_hls_origin_telemetry`).
It answers `503` while the feature is disabled.

```json
{
  "scope": "source",
  "dropped_events": 0,
  "queue_capacity": 1024,
  "inputs": [
    {
      "input_name": "provider-a",
      "availability": {
        "sample_count": 120, "window_secs": 300, "last_sample_age_ms": 800,
        "window_covered_ms": 299000, "ok": 118, "timeout": 1, "truncated": 1, "success_ratio": 0.98
      },
      "transfer_quality": {
        "sample_count": 118, "window_secs": 300, "last_sample_age_ms": 800,
        "network_ms_p50": 210, "network_ms_p95": 1900,
        "first_byte_ms_p50": 90, "first_byte_ms_p95": 400,
        "queue_wait_ms_p95": 5, "throughput_kbps": 6200,
        "cache_write_ms_p95": 12, "postprocess_ms_p95": 3, "cache_ready_ms_p95": 2000,
        "attempt_ms_p95_incl_failed": 9800
      }
    }
  ]
}
```

* `scope: "source"`: values describe origin fetches per input. They are **not** copied into client sessions and do not
  grow with the number of viewers. One origin fetch is one sample, retries are separate samples.
* `availability` and `transfer_quality` are separate. A fetch that returned HTTP 200 with a slow body counts as
  available but shows up in `transfer_quality`.
* Every `transfer_quality` value is `null` when there is no sample or the newest sample is older than
  `freshness_secs`. `availability` is not nulled by freshness: `success_ratio` stays set as long as the window holds
  samples, so check `availability.last_sample_age_ms`. `sample_count` and `last_sample_age_ms` tell you how much to
  trust a value. Never treat `null` as `0`.
* Cancelled fetches are counted but excluded from `success_ratio`.
* `network_ms` is the time from request start to the last body byte. Queue or lease wait (`queue_wait_ms`) is
  reported apart from it. Cache-side time is reported apart from it too: `cache_write_ms_p95`, `postprocess_ms_p95` and
  `cache_ready_ms_p95` (time until the object is servable from cache, measured from the start of the fetch).
* The transfer values (`network_ms_*`, `first_byte_ms_*`, `throughput_kbps`, cache-side values) only cover fetches that
  completed. A slow body that ends in a timeout or truncation is not in them. Read `attempt_ms_p95_incl_failed`
  (start to end over every attempt except cancelled ones) together with `availability.timeout` and
  `availability.truncated` to see stalls. It has its own freshness check, so a run of failures shows up even when no
  transfer completed.
* `window_covered_ms` is the age of the oldest sample. When the 4096-sample cap trims a busy window it is smaller than
  `window_secs`, so judge trends against it and not against `window_secs`.
* `dropped_events` counts records lost because the bounded queue was full. A non-zero value means the aggregates
  under-report.

## 3. What is measured

Per origin fetch attempt: queue wait, time to headers, first body byte, body completion, bytes read, HTTP status,
outcome (`ok`, `http_error`, `timeout`, `truncated`, `cancelled`, `transport_error`) and the phase where it failed
(`connect`, `headers`, `body`, `cache_write`, `postprocess`). Segment identity is an opaque salted hash that changes on
restart.

Limits to know:

* Manifest fetches are one request and body future, so headers, first byte and completion coincide.
* A provider URL failover inside one manifest attempt appears as one sample.
* Cache-side failures (capacity, commit) are reported as `transport_error` with phase `cache_write`.
* An attempt that fails before the request is sent (no free provider slot or account lease) sends nothing to the origin
  and produces no sample. Provider capacity problems therefore do not appear in `availability`.

## 4. Example: polling watchdog

```sh
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8901/api/v1/hls/origin-telemetry \
  | jq '.inputs[] | select(.transfer_quality.attempt_ms_p95_incl_failed != null and .transfer_quality.attempt_ms_p95_incl_failed > 4000)'
```

Act only on values with enough `sample_count` and a small `last_sample_age_ms`. After rotating the VPN exit, wait for
fresh samples before judging improvement.
