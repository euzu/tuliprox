# HLS Cache State Machines

This page documents the implemented runtime states for the Live HLS cache proxy.
It focuses on the shared `HlsSession`, the user-specific `HlsAccessLease`,
and the timing rules that connect them.

## Contents

- [High-Level Flow](#high-level-flow)
- [HlsAccessLease State Machine](#hlsaccesslease-state-machine)
  - [Access Lease Transitions](#access-lease-transitions)
- [Access Lease Reuse](#access-lease-reuse)
- [HlsSession State Model](#hlssession-state-model)
- [Origin Account Protection](#origin-account-protection)
  - [Soft-Overlap Candidate Selection](#soft-overlap-candidate-selection)
- [Manifest Acceptance And Recovery](#manifest-acceptance-and-recovery)
  - [Sequence Acceptance](#sequence-acceptance)
  - [Host Acceptance](#host-acceptance)
  - [Recovery Score](#recovery-score)
- [Manifest Recovery Retry And Burst](#manifest-recovery-retry-and-burst)
- [Transient Resource Delivery](#transient-resource-delivery)
- [Timings](#timings)
- [Request Effects](#request-effects)
- [Cleanup Rules](#cleanup-rules)

The HLS cache uses three different identifiers:

| Identifier | Scope | Meaning |
| :--- | :--- | :--- |
| `HlsSessionKey` | Shared content | Stable tuple of `input_id`, `hls`, and `stream_ref`. It does not include the origin URL, provider URL, username, or password. |
| `proxy_session_id` | Shared content URL | Opaque public token derived from `HlsSessionKey` and `reverse_proxy.rewrite_secret`. |
| `hls_access_lease_id` | User access URL | Opaque lookup key for one server-side `HlsAccessLease`. It is not a shared-content identity. |

## High-Level Flow

The entry request creates or reuses a short-lived pending access lease and redirects
the player to the canonical HLS cache path. The shared content session is created
or reused by the canonical manifest request after the access lease and admission
checks have passed.

```mermaid
flowchart TD
    A["Entry HLS request"] --> B["Authenticate user and prepare Tuliprox UserSession"]
    B --> C{"Reusable Pending lease for same family and proxy_session_id?"}
    C -->|yes| D["Reuse hls_access_lease_id"]
    C -->|no| E["Create new Pending HlsAccessLease"]
    D --> F["Redirect to canonical manifest"]
    E --> F
    F --> G["Validate access lease and admission"]
    G --> H["Create or reuse shared HlsSession"]
    H --> I["Fetch/render/serve manifest"]
    I --> J["Segment, MAP, or transient resource request"]
    J --> K["Activate or refresh access lease"]
    K --> L["Serve from cache, demand fetch, or transient passthrough"]
```

The canonical routes are:

```text
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/manifest.m3u8
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/<proxy_seq>.<ext>
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/map/<proxy_map_id>.<ext>
/hls/shared/live/<proxy_session_id>/<hls_access_lease_id>/r/<transient_resource_id>.<ext>
```

## HlsAccessLease State Machine

`HlsAccessLeaseId` is only the URL lookup key. The state lives in
`HlsAccessLeaseState`.

```mermaid
stateDiagram-v2
    [*] --> Pending: Entry redirect creates lease

    Pending --> Pending: Manifest touch extends valid_until
    Pending --> Activated: Segment, MAP, or /r resource request
    Pending --> Denied: Admission exhausted
    Pending --> Expired: valid_until <= now

    Activated --> Activated: Resource or active manifest touch
    Activated --> Idle: active_until <= now
    Activated --> Expired: valid_until <= now
    Activated --> Denied: Admission exhausted

    Idle --> Idle: Manifest touch extends valid_until
    Idle --> Activated: Later resource request reactivates lease
    Idle --> Expired: valid_until <= now
    Idle --> Denied: Admission exhausted

    Expired --> Removed: Lookup, lifecycle, or session cleanup
    Denied --> Removed: Lifecycle or session cleanup
    Removed --> [*]
```

### Access Lease Transitions

| From | To | Condition |
| :--- | :--- | :--- |
| none | `Pending` | Entry request prepares a new `HlsAccessLease` and redirects to the canonical manifest. |
| `Pending` | `Pending` | Canonical manifest validation succeeds. Manifest access does not activate the lease. |
| `Pending` | `Activated` | A segment, MAP, or transient resource request validates with `ResourceAccess`. |
| `Pending` | `Expired` | `valid_until_ms <= now_ms`, including the exact boundary. |
| `Pending` | `Denied` | Admission returns exhausted for the bound Tuliprox UserSession. |
| `Activated` | `Activated` | A resource request, or a manifest request for an already activated lease, slides the active and valid windows. |
| `Activated` | `Idle` | `active_until_ms <= now_ms` while `valid_until_ms > now_ms`. The bound UserSession stream reservation is released. |
| `Activated` | `Expired` | `valid_until_ms <= now_ms`. If it was active, the same cleanup path releases the UserSession stream reservation. |
| `Idle` | `Idle` | A valid manifest request can keep the lease entry alive without restarting media activity. |
| `Idle` | `Activated` | A later segment, MAP, or transient resource request reactivates the same lease. |
| `Idle` | `Expired` | `valid_until_ms <= now_ms`. |
| any usable state | `Denied` | Admission rejects the underlying Tuliprox UserSession as exhausted. |
| `Expired` or `Denied` | removed | Lifecycle processing, stale lookup, cache path reset, or session cleanup removes the store entry. |

Only `Pending` and `Activated` leases count as usable for background origin work.
An `Idle` lease can still validate its own manifest or resource URL, but it does
not keep prefetch or map-fetch workers active until a media request reactivates it.

## Access Lease Reuse

Entry-redirect reuse is only an idempotency window for duplicated player startup
requests. It must not collapse two real parallel playbacks onto one access lease.

A previous lease is reusable only when all conditions hold:

| Condition | Required value |
| :--- | :--- |
| `family_key` | Same user and client fingerprint family. |
| `proxy_session_id` | Same shared HLS session URL identity. |
| `state` | `Pending`. |
| age | `now_ms - issued_at_ms < 5000`. |

`last_seen_at_ms` does not extend the reuse window. Once a lease is `Activated`,
`Idle`, `Expired`, or `Denied`, a later entry request creates a new lease instead
of reusing the old one. Creating or activating a new lease does not invalidate
older leases from the same playback family.

## HlsSession State Model

`HlsSession` does not have one single `Active` or `Idle` enum. Its effective state
is composed from:

| Layer | Stored as | Meaning |
| :--- | :--- | :--- |
| Store presence | `HlsSessionStore` indexes | Whether the shared session exists. |
| Media mode | `HlsSessionMode` | Whether media is handled by the normal cache timeline or transient passthrough. |
| Activity | `HlsSessionActivity` | Last authorized manifest/media access, active lease count, origin work count, and origin work generation. |
| Origin binding | `HlsOriginAccountBindingMode` | Whether a provider account binding is active, speculative, or detached. |
| GC flag | `gc_marked_for_removal` | Temporary guard while a session is being removed. |

```mermaid
stateDiagram-v2
    [*] --> Missing
    Missing --> NormalCacheTimeline: Canonical manifest creates session
    NormalCacheTimeline --> NormalCacheTimeline: Normal origin manifest commit
    NormalCacheTimeline --> TransientPassthrough: Origin manifest requires transient handling
    TransientPassthrough --> TransientPassthrough: Later transient manifest commits

    NormalCacheTimeline --> Removing: Idle cleanup conditions hold
    TransientPassthrough --> Removing: Idle cleanup conditions hold
    Removing --> Missing: Store entry and cache directory removed
    Removing --> NormalCacheTimeline: Cleanup deferred for active temp files or new work
    Removing --> TransientPassthrough: Cleanup deferred for active temp files or new work
```

Session cleanup can run only when the session idle timeout has elapsed and the
session has no active origin work, segment fetches, map fetches, origin refresh,
prefetch queue entries, active cache readers, fetching segment/MAP objects, or
active transient resource readers.

## Origin Account Protection

Origin account protection is derived from the last successful media access. It is
the same for `NormalCacheTimeline` and `TransientPassthrough`.

```mermaid
stateDiagram-v2
    [*] --> NoMediaYet: No successful media response yet
    NoMediaYet --> HardActive: Segment, MAP, or /r response succeeds
    HardActive --> SoftActive: now > last_media + target_duration
    SoftActive --> Expired: now > hard_until + 2 * target_duration
    SoftActive --> HardActive: New media response succeeds
    Expired --> HardActive: New media response succeeds after reacquire
```

The timing uses `#EXT-X-TARGETDURATION` when known. If the session has not parsed
one yet, the fallback target duration is 15 seconds.

| Protection state | Condition |
| :--- | :--- |
| `NoMediaYet` | No segment, MAP, or transient resource response has succeeded yet. |
| `HardActive` | `now <= last_authorized_media_at_ms + target_duration`. |
| `SoftActive` | After the hard window, until another `2 * target_duration` has elapsed. |
| `Expired` | The soft window has elapsed. |

Hard-active sessions are not candidates for speculative account overlap. Soft-active
sessions may be displaced speculatively, and the original owner may reclaim within
the soft window.

### Soft-Overlap Candidate Selection

Soft-overlap is a controlled fallback when a new shared HLS session needs an origin
account and no free account is available through the normal provider lineup path.
It is not used for background-only origin work.

A session can become a soft-overlap candidate only when all conditions hold:

| Condition | Required value |
| :--- | :--- |
| Input | Same `input_name` as the new shared HLS session. |
| Binding mode | Existing binding is `Active`, not already `Speculative` or detached. |
| Protection state | `SoftActive`. `HardActive`, `NoMediaYet`, and `Expired` are excluded. |
| Origin work | No active origin work is currently running for the candidate session. |
| Account cooldown | The account is not inside the HLS overlap cooldown window. |
| Existing speculative use | The same account is not already used by another speculative HLS session. |

Eligible candidates are ordered by `last_authorized_media_at_ms`; the oldest
soft-active media session is considered first.

Before a soft-active session can be displaced, Tuliprox applies a dynamic delay.
The delay is based on the pressure between Tuliprox user connection capacity for
the target and origin account connection capacity for the input:

| Capacity signal | Source |
| :--- | :--- |
| `tuliprox_target_user_connection_capacity` | Maximum configured Tuliprox user connection capacity of targets that contain this input, also bounded by the currently active streams for this input. |
| `origin_input_account_connection_capacity` | Current provider lineup account connection capacity for this input. If the runtime lineup is not available, the input plus enabled aliases are used as a fallback. |

The computed delay is clamped between `1 * target_duration` and
`2 * target_duration`:

| Pressure | Soft-overlap delay |
| :--- | :--- |
| Tuliprox capacity is less than or equal to origin capacity | `2 * target_duration` |
| Tuliprox capacity is at least twice origin capacity | `1 * target_duration` |
| Between those points | Linear integer-scaled delay between the two bounds |

This means small account pools with many possible Tuliprox users can reclaim
soft-active sessions sooner, while low-pressure setups keep the additional
protection window.

When a speculative session is reclaimed by the original owner, or when the
speculative session survives the reclaim window and is promoted, Tuliprox starts
an account cooldown for that `input_name + account_name`. The cooldown lasts one
additional hard-active window from the transition time. During that cooldown, the
same account is not selected again as a soft-overlap candidate. This cooldown is
additive to the hard-active protection of the session that now owns the account.

## Manifest Acceptance And Recovery

After access-lease validation and admission, the canonical manifest path fetches
an origin manifest and evaluates whether it can advance the shared HLS session.
The same acceptance code is used by `NormalCacheTimeline` and
`TransientPassthrough`.

The policy tracks two independent signals:

| Signal | Source | Purpose |
| :--- | :--- | :--- |
| Effective manifest host | Actual redirect host when a redirect happened; otherwise the resolved request URL host for plain HTTP(S). A `provider://` mirror index alone is not a host-switch signal. | Prefer the already pinned redirect host and delay controlled host switches. |
| Origin highwater | `#EXT-X-MEDIA-SEQUENCE + visible origin segment count - 1`. | Detect forward progress, stalls, rollovers, and invalid jumps. |

```mermaid
flowchart LR
    A["Origin manifest fetched"] --> B["Parse timeline"]
    B --> C["Evaluate host and highwater"]
    C --> D{"Initial, same host, or unknown host?"}
    D -->|yes| E{"Sequence accepted?"}
    E -->|yes| F["Commit manifest and reset recovery counters"]
    E -->|no| G["Reject and enter HLS recovery retry"]
    D -->|other host| H["Store best host-switch candidate"]
    H --> I["Retry pinned host with HLS recovery policy"]
    I --> J{"Pinned host recovered?"}
    J -->|yes| F
    J -->|no| K{"Host-switch threshold reached?"}
    K -->|no| L["Keep candidate, retry pinned host later"]
    K -->|yes| M["Allow held candidate if sequence is accepted"]
    M --> F
```

### Sequence Acceptance

The sequence policy is strict while the session is `HardActive` or `SoftActive`.
It allows a rebase only when the session has no committed highwater yet, or when
account protection reports `NoMediaYet` or `Expired`. Rebase relaxes only the
sequence check; it does not bypass the host-switch policy.

| Relation | Condition | Result |
| :--- | :--- | :--- |
| `Rebase` | Rebase mode is allowed and the fetched manifest has an origin highwater. | Accept as a fresh continuity base. |
| `NoPreviousHighwater` | The session has no previous highwater. | Accept and initialize highwater. |
| `Next` | `origin_highwater == previous_highwater + 1`. | Accept, best score on the same redirect host. |
| `PlausibleForward` | `origin_highwater > previous_highwater` and the delta is within the forward window. | Accept. |
| `Same` | `origin_highwater == previous_highwater`. | Accept but count as no progress. |
| `RolloverCandidate` | `origin_highwater` moved backwards but is within the rollover window. | Accept and mark handoff discontinuity. |
| `NoOriginHighwater` | A previous highwater exists but the fetched manifest has no origin highwater. | Reject. |
| `ForwardTooFar` | Forward jump exceeds the allowed forward window. | Reject. |
| `Backward` | Backward jump is outside the rollover window. | Reject. |

The forward and rollover window is derived from the configured
`session_idle_timeout` divided by the manifest target duration, rounded up. If no
target duration is known, the same 15-second target-duration fallback as account
protection is used.

### Host Acceptance

The current redirect host is sticky. A manifest from another redirect host is
stored as a host-switch candidate, but the proxy first retries the pinned target
URL. The retry chain failure counter increments once after a complete pinned-host
recovery chain fails, not for every single manifest request inside that chain.

The host-switch threshold is:

```text
floor((3 + effective_strip_segments) / 2), clamped to 1..5
```

For `strip.mode = segments`, `effective_strip_segments` is `strip.value`. For
`strip.mode = seconds`, it is the current render gap in segments. Once the
threshold is reached, the held host-switch candidate may be committed, but only
if the same sequence-acceptance policy accepts it.

### Recovery Score

When more than one recovery candidate returns a manifest, candidates are sorted
by score first, then by higher origin highwater, then by lower candidate number.

| Score | Rank | Meaning |
| :--- | :---: | :--- |
| `same-host-next-sequence` | 100 | Same redirect host and exact next highwater. |
| `same-host-plausible-forward` | 90 | Same redirect host and plausible forward progress. |
| `same-host-rebase` | 85 | Same redirect host and rebase mode is active. |
| `other-host-next-sequence` | 75 | Other redirect host and exact next highwater. |
| `other-host-plausible-forward` | 65 | Other redirect host and plausible forward progress. |
| `other-host-rebase-candidate` | 60 | Other redirect host and rebase mode is active. |
| `same-host-rollover-candidate` | 50 | Same redirect host and plausible rollover. |
| `other-host-rollover-candidate` | 35 | Other redirect host and plausible rollover. |
| `same-host-unchanged` | 20 | Same redirect host but no highwater progress. |
| `other-host-unchanged` | 10 | Other redirect host and no highwater progress. |
| `rejected` | 0 | Candidate failed sequence acceptance or parsing. |

## Manifest Recovery Retry And Burst

The initial manifest download uses the global Tuliprox request, redirect, timeout,
and provider URL failover helpers. Once a manifest body was loaded but rejected by
HLS timeline acceptance, recovery switches to the HLS recovery policy. Recovery
uses the concrete pinned or resolved target URL and does not restart the full
`provider://` chain.

The fixed HLS manifest recovery retry policy is:

| Attempt | Base delay before attempt |
| :--- | :--- |
| 1 | `0ms` |
| 2 | `100ms` |
| 3 | `250ms` |
| 4 | `500ms` |
| 5 | `750ms` |

Each delay adds jitter from `0ms` through `100ms`. Retryable fetch errors and
manifest rejections both continue through this same five-attempt chain. A final
rejection returns `RetryExhausted` to the caller.

The optional `manifest_recovery_burst.level` affects only the first attempt of a
recovery chain. Later attempts always run with one candidate. Burst candidates
start against the same recovery target URL and are staggered by `100ms` per slot.

| Level | Slots | Requests per slot | Total first-attempt candidates |
| :--- | :---: | :---: | :---: |
| `off` | 1 | 1 | 1 |
| `friendly` | 2 | 1 | 2 |
| `cautious` | 3 | 1 | 3 |
| `balanced` | 4 | 1 | 4 |
| `intense` | 5 | 1 | 5 |
| `aggressive` | 6 | 1 | 6 |
| `beast` | 6 | 2 | 12 |

`beast` uses the same six time slots as `aggressive`, but launches two requests
per slot. It does not change scoring, acceptance, provider URL failover, or later
retry attempts.

The recovery logs use compact manifest-scoped messages:

```text
Manifest '<session>' attempting URL attempt <n> of <max>: <target> reason=<reason>
Manifest '<session>' attempting URL attempt <n> of <max> candidate <c> of <max>: <target> reason=<reason>
Manifest '<session>' candidate <c> of <max> scored: host=<host> highwater=<value> score=<score>
Manifest '<session>' candidate <c> of <max> rejected: host=<host> highwater=<value> reason=<reason>
Manifest '<session>' burst selected candidate <c> of <max>: host=<host> highwater=<value> score=<score>
Manifest '<session>' retry scheduled: status <reason> attempt <n> of <max> next_delay_ms=<delay>
```

## Transient Resource Delivery

Transient passthrough uses the same access lease lifecycle and admission checks as
normal cache timeline mode. Only media object handling is different.

There are two delivery paths inside `TransientPassthrough`: cacheable full-object
resources are fetched once and served from the HLS object cache, while keys and
non-cacheable range requests use direct passthrough. Both paths run after the
same lease validation and origin account preparation.

```mermaid
flowchart TD
    A["/r/<transient_resource_id> request"] --> B["Validate access lease and admission"]
    B --> C{"Valid transient resource ref?"}
    C -->|no| Z["404 or configured unavailable response"]
    C -->|yes| D{"Resource kind is Key?"}
    D -->|yes| E["Direct passthrough, no object cache"]
    D -->|no| F{"Ready object cache entry?"}
    F -->|yes| G["Serve cached object"]
    F -->|no| H{"Cacheable full-object request?"}
    H -->|no| E
    H -->|yes| I{"Fetch already in flight?"}
    I -->|yes| J["Wait for fetch, then serve or retry response"]
    I -->|no| K["Fetch and cache object"]
```

A transient request is treated as full-object cacheable when it has no `Range`
header or exactly `Range: bytes=0-`. Other range requests use direct passthrough.
Keys always use direct passthrough. Both cached transient object fetches and direct
transient passthrough use `origin_segment_timeout_ms` for origin body reads; direct
passthrough applies it while reading upstream chunks.

## Timings

| Timing | Default | Used for |
| :--- | :--- | :--- |
| Access lease reuse window | `5000ms` | Only duplicate entry redirects may reuse a still-`Pending` lease. |
| Access lease active window | `2 * target_duration` | How long an activated lease counts as active after last activation/touch. |
| Access lease validity window | `session_idle_timeout` | How long a lease remains valid before becoming `Expired`. |
| Session idle timeout | `300s` | When a session may be collected after last authorized manifest/media access. |
| Account hard-active window | `1 * target_duration` | Provider account is protected from speculative overlap. |
| Account soft-active window | additional `2 * target_duration` | Provider account can be speculatively displaced and reclaimed. |
| Soft-overlap eligibility delay | `1..2 * target_duration` | Dynamic delay before a soft-active session can be displaced, based on Tuliprox target user capacity versus origin account capacity. |
| Soft-overlap account cooldown | `1 * target_duration` after reclaim or promotion | Prevents the same account from being repeatedly selected as a soft-overlap candidate immediately after ownership changes. |
| Target-duration fallback | `15s` | Used before an origin manifest target duration has been parsed. |
| Origin manifest timeout | `3000ms` | Manifest origin fetch attempt timeout. |
| Manifest commit wait | `origin_manifest_timeout_ms + 250ms` | How long the canonical manifest path waits for a newly committed manifest. |
| Origin refresh interval | at least `1000ms`, fallback `2000ms` | Base interval is half of last segment duration or target duration. Successful refreshes without highwater progress apply `base * 0.5^N` down to `1000ms`; failures use the separate failure backoff. |
| Origin object body timeout | `10000ms` | Segment, MAP, transient object, and transient passthrough body reads. |
| Temporary transient retry-after | `1s` | Response for in-flight or retryable transient object cache misses. |
| HLS cache GC interval | `30s` | Background garbage collection cadence. |

Manifest timing logs include `progress=advanced|unchanged`,
`empty_refreshes=<n>`, and `next_refresh_in_s` after empty-refresh rampdown has
been applied.

## Request Effects

| Request | Validates access lease | Activates lease | Updates manifest activity | Updates media activity |
| :--- | :---: | :---: | :---: | :---: |
| Entry redirect | no existing lease required | no | no | no |
| Canonical manifest | yes | no | yes, after origin runtime is prepared | only when a committed manifest is served as playback media continuity |
| Segment | yes | yes | no | yes on `200` or `206` |
| MAP | yes | yes | no | yes on `200` or `206` |
| Transient `/r/...` resource | yes | yes | no | yes on successful response |

## Cleanup Rules

Access leases are removed when they expire, are denied and processed by lifecycle,
or when the owning `proxy_session_id` is removed. Session removal also removes
per-lease repair windows and related repair state.

Shared sessions can outlive individual user access leases as cache-only runtime
objects. They are removed only when the idle timeout and all safety conditions
allow cleanup.
