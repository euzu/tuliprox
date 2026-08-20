# DVR Operator Reference

This guide covers everything an operator needs to know to deploy, configure, and migrate to the
extended DVR. It is the single source of truth for the documentation that
[`config.md`](../configuration/config.md) and [`rest-api-cookbook.md`](../rest-api-cookbook.md)
consume.

## 1. Configuration reference

All new fields live under `video.download.recording` in the config file. Every field is optional;
defaults match the recommended values.

```yaml
video:
  download:
    recording:
      directory: recordings/         # default: <download-dir>/recordings
      timezone: Europe/Berlin         # default: UTC (IANA required)
      filename_template: "{channel}_{program_title}_{start_time}"
      default_pre_roll_secs: 0        # 0..=max_pre_roll_secs
      max_pre_roll_secs: 900          # ≤ 900 (15 min)
      default_post_roll_secs: 0       # 0..=max_post_roll_secs
      max_post_roll_secs: 1800        # ≤ 1800 (30 min)
      retention:
        keep_last_per_channel: 10     # > 0 when set
        delete_after_days: 30         # > 0 when set
      disk:
        high_water_percent: 85        # 0..=100
        low_water_percent: 70         # 0..=100 and < high_water_percent
        cleanup_interval_secs: 3600   # > 0
        safety_bytes: 1073741824      # > 0 (1 GiB)
      quota:
        default_private_bytes: 53687091200   # 50 GiB
        per_user_bytes:
          "web:user-uuid-1": 107374182400    # 100 GiB
        shared_bytes: 536870912000           # 500 GiB
      fallback_bytes_per_minute: 8388608     # 8 MiB/min, > 0
```

### 1.1 Validation rules

The validation list, applied at `VideoConfigDto::prepare`:

- `keep_last_per_channel > 0` when set.
- `delete_after_days > 0` when set.
- `cleanup_interval_secs > 0`.
- `low_water_percent < high_water_percent` and both in `0..=100`.
- `safety_bytes > 0`.
- `default_private_bytes`, `per_user_bytes[*]`, `shared_bytes` all `> 0` when set.
- `fallback_bytes_per_minute > 0`.
- `timezone` is a valid IANA zone (`chrono_tz::Tz::parse`).
- `filename_template` contains at least one known placeholder and is no longer than 240 UTF-8 bytes.

Absent quotas and absent retention values disable the corresponding policy (no limit).

### 1.2 Disk watermark semantics

`high_water_percent` and `low_water_percent` are **used-space percentages** on the filesystem
containing the canonical recording root. The retention worker uses them with hysteresis: when
used-space ≥ `high_water_percent`, the worker deletes oldest completed recordings until
used-space ≤ `low_water_percent` (or the eligible list is exhausted). `cleanup_interval_secs` is
the wall-clock interval between passes; the worker uses a cancellation-aware Tokio interval and
will not overlap.

## 2. Recording directory layout and immutable IDs

The recording root is the path configured in `recording.directory` (default `<download-dir>/recordings`).
The runtime resolves the path under `<recording-root>/users/<owner-id>/<rel>` for private
recordings and `<recording-root>/shared/<rel>` for shared recordings. The `<owner-id>` is the
authenticated `UserId` from the JWT subject claim (UUID v4 hex with `web:` / `api:` /
`builtin:admin` namespaces). The `<rel>` is the collision-safe relative path the queue-mutation
boundary reserved.

The directory tree:

```text
<recording-root>/
  users/
    <owner-id>/<rel>     # private
  shared/<rel>           # shared
```

`<owner-id>` is **immutable** for the lifetime of the recording record. The runtime never derives
directory names from usernames, channels, programme titles, or any other mutable identifier. The
path is canonicalized at file open time against the recording root's file descriptor; any path
that escapes the root (via `..`, symlinks, or absolute paths) is rejected with
`recording_unsafe_path`.

## 3. Filename placeholders

The supported placeholders:

- `{channel}` — channel display name; falls back to the stable channel id when the display name
  is missing.
- `{program_title}` — programme title; sanitized via the existing filename sanitizer.
- `{start_time}` — programme start in the configured timezone, rendered as `YYYY-MM-DD_HH-mm`.
- `{end_time}` — same, programme end.
- `{episode}` — renders `SxxExx` when both season and episode numbers exist; otherwise empty.
- `{owner}` — sanitized owner display name. **Filename only; never a directory component.** The
  `{owner}` placeholder is the only place the username-derived content may appear in the on-disk
  path.

The final stem is capped at 240 UTF-8 bytes without splitting a code point. When the sanitized
stem is empty, the runtime falls back to the recording task id.

## 4. Lifecycle and restart behavior

Every recording goes through three persistence states:

1. **Partial path** — `partial_relative_path` is set; the `O_CREAT | O_EXCL | O_NOFOLLOW` open
   guarantees an attacker-prepared symlink at the partial path is rejected.
2. **Finalize** — atomic rename from partial to final; the runtime refuses to clobber any
   pre-existing final file (including symlinks).
3. **Complete** — `mutate` sets `Completed`, stamps `measured_bytes`, `completed_at`, and clears
   `reserved_bytes`.

A crash at any point is recoverable:

- Active + valid final file → normalize to `Completed`.
- Active + valid partial file → normalize to terminal `Failed`, retain partial.
- Active + no owned file → normalize to terminal `Failed`.
- Unsafe path / type → log a security-category error, normalize to previous terminal state, do
  not open.

## 5. Quota charge-by-state

The quota ledger charges the bytes below per `DownloadState`:

| State | Charged bytes |
|---|---:|
| `Scheduled` / `Queued` / `WaitingForCapacity` / `RetryWaiting` | `reserved_bytes` |
| `Downloading` | `max(reserved_bytes, measured_bytes)` |
| `Completed` | final `measured_bytes` |
| `Failed` / `Cancelled` with partial file | partial `measured_bytes` |
| `Deleting` | charge of the saved terminal state until removal commits |

A task is counted exactly once. Private pools key on `RecordingOwner::User(uid)`; shared pools key
on `RecordingVisibility::Shared`; `LegacyAdmin` recordings count toward the shared pool. Per-user
overrides beat the configured default; an absent limit is unlimited.

### 5.1 Active overrun policy

Version one does **not** terminate an active recording because it grew beyond quota. `would_exceed`
is admission-only. When the measured partial size exceeds the reservation, the charge is
`max(reserved, measured)`; the next `would_exceed` call rejects new admissions until the recording
finishes or is deleted. The user-visible DTO surfaces an `Overrun` warning so operators can grant
more quota or delete the recording.

### 5.2 Unknown bitrate

When the bitrate is unknown at create time, the reservation is
`duration_minutes × fallback_bytes_per_minute` (default 8 MiB). The DTO surfaces an
`UnknownBitrate` warning. The runtime re-reserves with the measured rate as soon as the worker
starts.

## 6. Disk admission

The disk admission path:

```text
headroom = free_bytes - safety_bytes - active_disk_reservations
admit    = charge <= headroom
```

`free_bytes_for(path)` is `statvfs` (Unix) or `GetDiskFreeSpaceExW` (Windows) keyed on the supplied
path's mount. The pre-start flow always passes the canonical recording root so the measurement is
on the same filesystem the file will live on. Two starts cannot consume the same headroom — the
active reservation is serialized through the queue-mutation boundary.

## 7. Safe deletion guarantees

Deletions use a persisted two-phase operation:

1. **`begin_deletion`** runs inside the queue-mutation boundary. It stamps
   `recording.deleting_previous_state = Some(prior)` (the prior terminal state from
   `Completed` / `Failed` / `Cancelled`) and zeros the byte counts.
2. **`execute_deletion`** runs **after** the boundary. It inspects the file with `O_NOFOLLOW`
   (no symlink dereference) and refuses any path that is not a safe regular file. It removes the
   file. Missing files are idempotent success.
3. **`finalize_deletion`** runs inside a fresh boundary. It removes the task from the queue and
   clears `deleting_previous_state`.

Startup recovery:

- `Deleting` + missing file → finish task removal.
- `Deleting` + existing valid regular file → restore previous state, clear the marker.
- `Deleting` + unsafe path or non-regular file → restore previous state, log a security-category
  error.

The runtime uses the strongest available primitives on each platform. On Linux the helper uses
`O_NOFOLLOW` for descriptor-relative containment. On other platforms the helper falls back to
no-follow metadata checks; the race window is documented in the rustdoc and the source.

## 8. Authorization matrix

| Operation | Private recording | Shared recording | `LegacyAdmin` | Orphan |
|---|---|---|---|---|
| Read / Playback / Download | owner with `recording.read` | anyone with `recording.read` | admin only | admin only |
| Create private | user with `recording.write` | n/a | admin only | n/a |
| Create shared | rejected (admin only) | admin + `recording.write` | admin only | n/a |
| Edit / Cancel / Delete | owner + `recording.write` | admin + `recording.write` | admin only | n/a |
| Manage recurring rule | owner + `recording.write` | admin + `recording.write` | admin only | n/a |
| `SystemRetentionDelete` | ownership bypassed; state-gated | ownership bypassed; state-gated | ownership bypassed; state-gated | n/a |
| Orphan catalog | n/a | n/a | n/a | admin only |

Administrators **do not** implicitly receive another regular user's private recording content. The
private owner is the only non-administrator allowed to read it. Administrative access is read-only
for diagnosis; mutations require either the `SystemRetentionDelete` action (which the retention
worker is the only legitimate caller of) or the appropriate `recording.write` + ownership
combination.

Orphan catalog entries (recordings whose target/input no longer matches a configured source) are
visible only to administrators with `recording.read`. The path is never exposed; an opaque orphan
id is generated per discovery.

## 9. Identity-registry bootstrap

The identity registry is `web_user_ids.json` in the storage directory. The startup sequence is:

1. Pre-scan `downloads_state.json` for `RecordingOwner::User(_)` entries (without the registry
   loaded).
2. Load the existing registry (if any).
3. Initialize the registry **only** when no persisted real owner exists. New `UserId`s are
   generated for any username that lacks one.
4. Fail closed on missing / corrupt registry when real owners exist. The server does not generate
   replacement IDs in this case; the operator must restore the registry or run an explicit rename
   migration.
5. Sync current principals (insert a new `UserId` for any username that lacks one).
6. Run the full queue load + normalization.

The built-in administrator is the reserved subject id `builtin:admin` (constant). Operators do not
create an entry for it.

## 10. Token refresh on permission schema bump

`Claims` carries `subject_id: Option<UserId>` and `permission_schema_version: u16`. The constant
`CURRENT_PERMISSION_SCHEMA_VERSION` is the source of truth. When the schema changes, bump the
constant; pre-bump tokens become stale:

- `authenticator::validate_token_version` returns `AuthError::StaleSchema` for older versions.
- The HTTP layer emits a 401 with header `X-Token-Refresh: required`.
- The frontend's `RecordingError::TokenRefreshRequired` and the generic auth refresh handler
  redirect the user back to the sign-in flow.

Operators do **not** need to manually invalidate tokens on a schema bump. Existing user records in
`web_user_ids.json` are preserved; only the `subject_id` mapping for current usernames is
recomputed if missing.

## 11. Deprecated `/file/record` behavior

The legacy `POST /file/record` route is **deprecated** and delegates to
`RecordingService::create_recording` for administrators only. Non-administrators receive a 403 —
the deprecated route does not bypass the new policy.

The migration:

- **Frontend code**: switch from `downloads_service::queue_recording` to
  `recording_service::RecordingService::create_task`. The new client submits
  `RecordingSourceInput` (target_id + virtual_id + input_name) and `CreateRecordingTaskRequest`,
  never a free-form URL.
- **Operator code**: the legacy route is documented as deprecated and will be removed in the
  next major release. New automations should use `/api/v1/recording/tasks` (and
  `/api/v1/recording/rules` for recurring rules).

## 12. Scoped REST and WebSocket APIs

The recording surface is exposed under `/api/v1/recording`:

```text
GET    /api/v1/recording/tasks
POST   /api/v1/recording/tasks
PATCH  /api/v1/recording/tasks/{id}
POST   /api/v1/recording/tasks/{id}/cancel
DELETE /api/v1/recording/tasks/{id}
POST   /api/v1/recording/conflicts/preview
GET    /api/v1/recording/quota
GET    /api/v1/recording/rules
POST   /api/v1/recording/rules
PATCH  /api/v1/recording/rules/{id}
DELETE /api/v1/recording/rules/{id}?future=retain|cancel
```

The `tasks` payload is a per-session filtered snapshot. The WebSocket protocol carries
`RecordingSnapshotRequest`, `RecordingSnapshotResponse { revision, tasks }`, and
`RecordingDeltaResponse { revision, tasks }`. The `revision` field is the monotonic
`QueueRevision`; clients that detect a revision gap must request a fresh filtered snapshot.
A dedicated `RecordingRulesChanged` notification (no payload) is broadcast on every rule
mutation so the rules view can refresh without polling.

Filtering is server-side: private events go only to the owner session, shared events go to anyone
with `recording.read`, `LegacyAdmin` events go only to administrator sessions. Generic download
events (`DownloadsResponse`, `DownloadsDeltaResponse`) contain no recording tasks.

## 13. Conflict-preview advisory semantics

The conflict analyzer is **advisory** — runtime capacity is authoritative. The three-bucket
classification:

- `NoKnownConflict` — every segment of the candidate's padded interval is under capacity.
- `PossibleCapacityWait` — some segments are over.
- `LikelyMissedWindow` — every segment is over.

Create / edit operations return the preview's severity as a warning; the request still succeeds
when the hard checks (authorization, source, interval, padding, quota, path reservation) pass.
The preview endpoint accepts the same `CreateRecordingTaskRequest` and returns the preview.

Privacy: the preview never returns another task's id, title, channel, filename, or rule data.
Logs and the response only carry the provider scope, anonymized interval, and severity.

## 14. Recurring-rule matching, DST, and reconciliation

### 14.1 NewEpisode matching

The matching order is stable series id first, normalized title as a fallback. Explicit `Repeat`
airing is excluded when `exclude_repeat = true` (the default). `Unknown` airing is treated as
new. The UI surfaces the title-fallback limitation when the EPG does not publish a stable
series id.

### 14.2 WeeklyTimeslot matching

`weekday` is `1..=7` (Monday = 1, Sunday = 7). `local_start_time` is `HH:MM`. `timezone` is an
IANA zone. The scheduler handles DST:

- Ambiguous local time (fall-back) → the earlier instant.
- Nonexistent local time (spring-forward) → advance 1 hour at a time up to 4 hours.

The UI surfaces the DST + IANA behavior next to the timezone input.

### 14.3 Cross-store reconciliation

Two stores can drift when one commit succeeds and the other fails. The reconciliation pass
produces a list of `ReconcileAction`s the caller applies under the queue-mutation boundary. The
truth table:

| Situation | Action |
|---|---|
| Materialized task, no `Scheduled` tombstone | `AddScheduledTombstone` |
| `Scheduled` tombstone, no task, rule enabled | `Materialize` |
| `Cancelled` tombstone, eligible inactive task | `Finalize` |
| `Cancelled` tombstone, **active** task | `ConflictingIntent` (log, manual resolution) |
| `Completed` tombstone, any task | suppress (no rematerialize) |
| Task terminal, tombstone `Scheduled` | `UpdateTombstone { kind: Completed }` |
| Tombstone past `expires_at` | `PruneTombstone` |
| Disabled rule | skip (no rematerialize) |
| Deleted rule | leave tombstone until expiry |

Tombstones are retained for the longer of the EPG horizon and the 14-day minimum horizon
(`MIN_TOMBSTONE_HORIZON_SECS`). The fixed cross-store lock order is
`queue mutation boundary → rule repository mutation`.

### 14.4 Delete with retain / cancel

`DELETE /api/v1/recording/rules/{id}?future=retain|cancel`:

- `retain` — set the rule's `enabled = false`. Existing tasks keep their `rule_id` and
  `occurrence_key` for historical provenance. The scheduler stops materializing future
  occurrences.
- `cancel` — same as `retain` plus cancel only future inactive occurrences. Active recordings
  are **never** auto-cancelled by rule deletion; the operator must resolve manually.

## 15. Best-effort at-most-once notification attempts

The notification adapter follows the at-most-once protocol:

1. The queue-mutation boundary persists a `NotificationMarker` for the lifecycle event
   (`Started` / `Completed` / `Failed`) in the same transaction as the state transition.
2. After the transaction commits, the adapter enqueues a post-commit dispatch.
3. Delivery is attempted at most once per marker.
4. After restart, the adapter checks the markers; if the marker is already present, no further
   action is taken.
5. A crash after marker commit and before delivery may lose the notification. This is
   acceptable.

The routing decision:

- `Shared` → deliver to global channels.
- `Private` + `LegacyAdmin` owner → deliver.
- `Private` + administrator owner → deliver.
- `Private` + regular user owner → suppress.

Missing messaging configuration is a no-op; the adapter logs the dispatch decision and returns.

## 16. Migration checklist

1. **Stop or quiesce** recording activity. Cancel active recordings and let the queue drain.
2. **Back up** the existing config, `downloads_state.json`, user / auth config, and messaging
   config.
3. **Deploy** the version with additive normalization. No config changes are required for the
   existing flows to keep working.
4. **Back up `web_user_ids.json`** after the first successful start. The bootstrap writes the
   file automatically; the operator should preserve it across restarts.
5. **Grant `recording.read` / `recording.write`** explicitly to the user groups that need them.
   The `permissions: 65535` legacy config does **not** implicitly grant the new bits.
6. **Refresh old tokens**. The schema bump forces a token refresh; pre-bump tokens get an
   `X-Token-Refresh: required` 401. Users sign in again to receive the current claims.
7. **Verify the recording root and free space** with `statvfs` (Linux) /
   `GetDiskFreeSpaceExW` (Windows). Confirm the `safety_bytes` is at least 1 GiB.
8. **Verify legacy recordings and paths**. The pre-Phase-1 `file_dir` / `file_path` fields
   normalize to private `LegacyAdmin` recordings. Confirm the existing media files are within
   the configured recording root or the legacy download root before enabling retention.
9. **Test one private and one shared recording** end-to-end before enabling the retention
   worker in production.
10. **Enable retention / quotas gradually**. Start with `delete_after_days` only; add
    `keep_last_per_channel` once the channel count is stable; add disk watermarks once the
    free-space baseline is known.
11. **Wire the `/api/v1/recording` routes** in the frontends that need them. The new form
    component (`recording_form`) is the single source of truth for both Playlist Explorer and
    EPG.

## 17. Acceptance scenarios (full sweep)

The acceptance scenarios the operator should verify before declaring the migration done:

1. Old configuration and `downloads_state.json` load without data loss.
2. Invalid recording kind / metadata combinations fail with `recording_invalid_state` or
   `recording_invalid_source`.
3. Queue persistence failure leaves the state and revision unchanged and emits no delta.
4. WebSocket revision gaps trigger a filtered resnapshot.
5. A private recording is invisible to a second user in tasks, deltas, catalog, playback,
   conflicts, quota, and logs.
6. Administrators can create shared recordings but do not see another user's private recordings
   through ordinary endpoints.
7. New APIs cannot submit raw URLs, owner IDs, absolute paths, or filenames — the wire shape
   is server-owned identifiers only.
8. Worker source re-resolution rejects stale / tampered source ownership
   (`recording_invalid_source`).
9. Partial files are not clobbered; finalization never overwrites an external file.
10. Crash recovery after each worker lifecycle point is deterministic and does not replay a
    missed live window.
11. Completed / failed / cancelled deletion removes only the task-owned final / partial file
    safely.
12. A deletion failure preserves the task and file.
13. Successful file removal plus final persistence failure remains recoverable as `Deleting`.
14. Symlink, non-regular, and containment attacks are rejected with `recording_unsafe_path`.
15. Filename templates sanitize, truncate, and reserve collisions safely, including the
    `{owner}` filename-only exception.
16. Private and shared quota pools are independent under concurrent create and start operations.
17. Active recordings remain conservatively charged; measured growth cannot create false free
    quota.
18. Disk safety and retention use the recording-root filesystem.
19. Retention never deletes generic downloads, active recordings, partials, unsafe legacy files,
    or orphan files.
20. DVR entries never enter Movies / Series or global user-independent caches.
21. Every playback / range / download open is authorized again.
22. EPG and Playlist Explorer use one form / one API.
23. Currently-airing scheduling reserves only the remaining duration and rejects elapsed windows.
24. Padding changes execution, quota, and conflicts consistently.
25. Editing succeeds only in allowed upcoming states and rolls back completely on persistence /
    quota / path failure.
26. Conflict warnings follow the deterministic classification and redact private metadata.
27. New-episode / weekly rules materialize only in horizon and survive DST / restart.
28. Task / tombstone reconciliation is idempotent after every injected cross-store failure.
29. Deleted, cancelled, and completed occurrences are not recreated within the tombstone horizon.
30. Notifications make no more than one external attempt per committed marker.
31. Missing / disabled messaging never fails a recording.
32. Production Rust remains free of `unwrap`, `expect`, and `panic` additions.

If a command or scenario fails, the operator records the exact command and output, fixes the
smallest root cause, reruns the focused test, and then the full relevant phase gate. Unexplained
known failures do not count as completion.
