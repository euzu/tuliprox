# DVR Operator Reference

This guide covers everything an operator needs to know to deploy, configure, and manage the
DVR. It is the single source of truth for the documentation that
[`config.md`](../configuration/config.md) and [`rest-api-cookbook.md`](../rest-api-cookbook.md)
consume.

## 1. Configuration reference

All new fields live under `video.download.recording` in the config file. Every field is optional;
defaults match the recommended values.

```yaml
video:
  download:
    recording:
      enabled: true                   # default: true; false stops every DVR supervisor
      container_format: mpegts        # mpegts (default) | matroska | mp4
      directory: recordings/          # default: <download-dir>/recordings
      timezone: Europe/Berlin         # default: UTC (IANA required)
      filename_template: "{channel}_{program_title}_{start_time}"
      default_pre_roll_secs: 0        # 0..=max_pre_roll_secs
      max_pre_roll_secs: 900          # ≤ 900 (15 min)
      default_post_roll_secs: 0       # 0..=max_post_roll_secs
      max_post_roll_secs: 1800        # ≤ 1800 (30 min)
      retention:
        keep_last_per_channel: 10     # > 0 when set
        delete_after_days: 30         # > 0 when set
        sweep_interval_secs: 3600     # default 3600; age/count sweep cadence
      disk:
        high_water_percent: 85        # 0..=100
        low_water_percent: 70         # 0..=100 and < high_water_percent
        cleanup_interval_secs: 3600   # > 0; watermark-check cadence
        safety_bytes: 1073741824      # > 0 (1 GiB)
      quota:
        default_private_bytes: 53687091200   # 50 GiB
        per_user_bytes:
          "web:user-uuid-1": 107374182400    # 100 GiB
        shared_bytes: 536870912000           # 500 GiB
      notifications:
        outbox_buffer: 1024           # default 1024; in-memory queue depth
        max_attempts: 6               # default 6; then dead-lettered
        backoff_initial_secs: 5       # default 5
        backoff_max_secs: 900         # default 900 (15 min)
      fallback_bytes_per_minute: 8388608     # 8 MiB/min, > 0
```

| Field                                | Default  | Range                       | Restart required | Effect                                                                                                                                                                                                                                                              |
|--------------------------------------|----------|-----------------------------|------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                            | `true`   | bool                        | no               | `false` skips reconciliation, retention, and the notification outbox at startup, makes running supervisors idle on their next tick, answers every `/api/v1/recording/**` route with `501 recording_disabled`, and hides the DVR entries from the web UI navigation. |
| `container_format`                   | `mpegts` | `mpegts`, `matroska`, `mp4` | no               | The `-f` muxer ffmpeg writes. `mpegts` survives truncation, so a recording killed mid-stream still plays — prefer it unless the source codecs need another container. Applies to recordings that start after the change.                                            |
| `retention.sweep_interval_secs`      | `3600`   | > 0                         | no               | Cadence of the age/count sweep. Independent of `disk.cleanup_interval_secs`.                                                                                                                                                                                        |
| `disk.cleanup_interval_secs`         | `3600`   | > 0                         | no               | Cadence of the supervisor's tick, and therefore of the watermark check. Measurements are floored at one per 30 s.                                                                                                                                                   |
| `notifications.outbox_buffer`        | `1024`   | ≥ 1                         | **yes**          | Channel capacity between the recorder and the outbox worker. Fixed when the worker starts.                                                                                                                                                                          |
| `notifications.max_attempts`         | `6`      | ≥ 1                         | no               | Delivery attempts per notification before it is dead-lettered.                                                                                                                                                                                                      |
| `notifications.backoff_initial_secs` | `5`      | ≥ 1                         | no               | First retry delay; doubles per attempt.                                                                                                                                                                                                                             |
| `notifications.backoff_max_secs`     | `900`    | ≥ `backoff_initial_secs`    | no               | Ceiling for the doubling.                                                                                                                                                                                                                                           |

Choosing values:

- **Private-only home use** — leave `quota` unset, set `retention.keep_last_per_channel` to taste,
  and leave `disk` unset if the recording filesystem is dedicated.
- **Shared family install** — set `quota.shared_bytes` and `disk.{high,low}_water_percent` so a
  full disk degrades into retention rather than failed recordings.
- **Multi-tenant** — set `quota.default_private_bytes` plus per-user overrides, and keep
  `notifications.max_attempts` low so a broken webhook does not accumulate a backlog.

### 1.0.1 ⚠️ Retention policy warning

**Retention deletes recordings.** If you are upgrading an existing install or reconfiguring
retention, whatever `retention` and `disk` values are in your config take effect on the next
supervisor sweep. If they were set optimistically or copied from an example, the first sweep may
delete recordings you expected to keep.

Before restarting after a configuration change:

1. **Read back your effective policy.** Check `video.download.recording.retention` and
   `video.download.recording.disk` in `config.yml`.
2. **Work out what would be deleted.** `keep_last_per_channel: N` keeps the *N most recent*
   recordings per (owner, channel) and deletes the rest. `delete_after_days: N` deletes anything
   whose `completed_at` is more than N days old. The two are a **union**, not an intersection —
   a recording matching either policy is deleted.
3. **If you are unsure, start with retention off.** Remove the `retention` block (or the
   individual keys) and set `enabled: true` with no policy. Nothing is deleted, and you can
   enable a policy deliberately once you have looked at the library.
4. **Back up** `downloads_state.json` and the recording directory.

There is no dry-run mode. Deletions are logged under the `recording::audit` target with a
`recording_retention_delete` line and a reason (`Age`, `Count`, or `watermark`), so
`grep recording_retention_delete` after the first sweep tells you exactly what went.

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

Free space is measured **once per pass**, not once per deletion. The stop condition folds in the
bytes the pass has already reclaimed, so a pass deletes just enough recordings to reach the low
watermark and then stops. Only the recording root's own filesystem is measured — never
`storage_dir` or the generic download directory, which may sit on a different mount.

### 1.3 What changed: supervisors now run

Three background supervisors now actually execute their work. They existed as decision layers
before but were never started, so the DVR worked on the happy path and silently skipped
everything else. The consequences of switching them on are all things this guide describes, but
they happen now where they did not before:

| Supervisor               | What now happens                                                                                                              |
|--------------------------|-------------------------------------------------------------------------------------------------------------------------------|
| Startup reconciliation   | Recordings stuck in `Deleting` from an earlier crash are finished or restored on boot. Orphaned rule tombstones are repaired. |
| Retention                | Age, count, and disk-watermark deletion begin. See the retention warning above.                                               |
| Notification outbox      | Lifecycle notifications are retried and persisted instead of being dropped on transient error.                                |

### 1.4 Supervisors

Three background supervisors implement the behaviour described in the rest of this document. All
three are started once the HTTP listener is bound, and all three honour the `downloads`
cancellation token, so a config reload stops and restarts them cleanly.

| Supervisor               | Cadence                                                                                 | Responsibility                                                                              |
|--------------------------|-----------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------|
| Startup reconciliation   | once at boot, before the rule scheduler                                                 | Finishes or undoes deletions interrupted by a crash; repairs queue/rule-store drift.        |
| Retention                | `disk.cleanup_interval_secs` tick, policy sweep every `retention.sweep_interval_secs`   | Age, count, and watermark deletion — all through the single `system_retention_delete` path. |
| Notification outbox      | event-driven, with per-entry retry timers                                               | Durable lifecycle-notification delivery with per-channel retry and dead-lettering.          |

Passes never overlap: a tick that arrives while the previous pass is still deleting is skipped.

`GET /api/v1/recording/health` (administrator only) reports each supervisor's last-tick timestamp,
the outbox depth, and the dead-letter count, so liveness can be checked without reading the log:

```json
{
  "enabled": true,
  "server_time": 1700003600,
  "reconciliation_last_run": 1700000000,
  "retention_last_tick": 1700003400,
  "retention_sweep_interval_secs": 3600,
  "notification_last_drain": 1700003100,
  "notification_outbox_depth": 0,
  "notification_dead_lettered": 0,
  "queue_revision": 412
}
```

A `null` timestamp means that supervisor has never completed a pass. Compare `retention_last_tick`
against `server_time` and `disk.cleanup_interval_secs` to detect a stalled sweep.

### 1.5 Support diagnostics

`bin/dvr_doctor.sh` collects everything above plus the on-disk state into one dump suitable for a
support ticket. It is read-only and never touches config, the queue, or a recording.

```bash
bin/dvr_doctor.sh --token "$ADMIN_TOKEN"
bin/dvr_doctor.sh --url https://tuliprox.example --token "$ADMIN_TOKEN" --storage-dir /opt/tuliprox/data
```

It reports supervisor health, the effective `recording` config block, quota, and aggregate
summaries of `downloads_state.json`, `recording_rules.json`, and the notification outbox —
including a `stuck_deleting` count and the set of channels the outbox is still retrying. The
summaries are deliberately aggregate: no titles, filenames, or owner ids are printed, because a
diagnostics dump gets pasted into tickets. The health and config sections need an administrator
token; the on-disk sections work without one.

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

| State                                                                     |                                            Charged bytes |
|---------------------------------------------------------------------------|---------------------------------------------------------:|
| `Scheduled` / `Queued` / `WaitingForCapacity` / `RetryWaiting` / `Paused` |                                       `reserved_bytes`   |
| `Downloading`                                                             |                    `max(reserved_bytes, measured_bytes)` |
| `Completed`                                                               |                                   final `measured_bytes` |
| `Failed` / `Cancelled` with partial file                                  |                                 partial `measured_bytes` |

A task that is mid-deletion carries `deleting_previous_state = Some(prior)` and is charged the
same bytes as the prior terminal state (`reserved_bytes` or `measured_bytes` depending on what
`prior` was); the field replaces the historical `DownloadState::Deleting` variant, which the
runtime no longer carries. The charge drops to zero only when `finalize_deletion` removes the
task from the queue.

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

Deletions use a persisted two-phase operation. The runtime carries the deletion intent in
`recording.deleting_previous_state: Option<DeletingPreviousState>` rather than as a
`DownloadState` variant — every terminal task that is mid-deletion stays in its prior state
(`Completed` / `Failed` / `Cancelled`) but carries the marker, which is what the rest of this
section means by "the task is in the deleting phase".

1. **`begin_deletion`** runs inside the queue-mutation boundary. It stamps
   `recording.deleting_previous_state = Some(prior)` (the prior terminal state) and zeros the
   byte counts.
2. **`execute_deletion`** runs **after** the boundary. It inspects the path with
   `symlink_metadata` (never `metadata`), so a symlink is seen as a symlink and refused rather
   than dereferenced, and removes the file. Missing files are idempotent success.
3. **`finalize_deletion`** runs inside a fresh boundary. It removes the task from the queue and
   clears `deleting_previous_state`.

Startup recovery (any task whose `deleting_previous_state` is `Some(_)`):

- `deleting_previous_state = Some(_)` + missing file → finish task removal.
- `deleting_previous_state = Some(_)` + existing valid regular file inside the recording root
  → restore the prior terminal state, clear the marker.
- `deleting_previous_state = Some(_)` + unsafe path or non-regular file → restore the prior
  state, log `recording_reconciliation_unsafe_path`, leave the file alone.

### 7.1 Portability of the path guarantees

The four guarantees — no symlink is followed, no existing file is clobbered, the publish is
atomic, nothing escapes the recording root — are built from portable primitives and hold
identically on every supported target:

| Guarantee                           | Primitive                                                                                                      | Portable? |
|-------------------------------------|----------------------------------------------------------------------------------------------------------------|-----------|
| No symlink followed on inspection   | `symlink_metadata` (never `metadata`)                                                                          | yes       |
| No existing file clobbered          | `create_new` → `O_CREAT\|O_EXCL` / `CREATE_NEW`; both fail on an existing entry *including a dangling symlink* | yes       |
| Atomic publish                      | `rename`, after a no-follow existence check on the destination                                                 | yes       |
| Contained in the recording root     | component validation plus an owner-id component check, before any syscall                                      | yes       |

Only one call has a platform-specific branch: `open_partial_no_clobber` additionally passes
`O_NOFOLLOW` on Unix. That is **defense in depth, not the mechanism** — the no-clobber property
already comes from `create_new`. `openat2` with `RESOLVE_BENEATH` / `RESOLVE_NO_SYMLINKS` would be
Linux-only and is deliberately not used.

Earlier revisions carried a blanket `#![cfg(unix)]` on the path helper, which removed the module
wholesale on Windows and left every caller with unresolved imports — the DVR did not build on
Windows at all. The gate is now scoped to the single `O_NOFOLLOW` line. Tests that need to
*create* a symlink stay Unix-only (Windows requires developer mode or elevation for that); the
behaviour they cover is asserted portably by the no-clobber tests.

## 8. Authorization matrix

| Operation                    | Private recording                 | Shared recording                 | `LegacyAdmin`                   | Orphan       |
|------------------------------|-----------------------------------|----------------------------------|---------------------------------|--------------|
| Read / Playback / Download   | owner with `recording.read`       | anyone with `recording.read`     | admin only                      | admin only   |
| Create private               | user with `recording.write`       | n/a                              | admin only                      | n/a          |
| Create shared                | rejected (admin only)             | admin + `recording.write`        | admin only                      | n/a          |
| Edit / Cancel / Delete       | owner + `recording.write`         | admin + `recording.write`        | admin only                      | n/a          |
| Manage recurring rule        | owner + `recording.write`         | admin + `recording.write`        | admin only                      | n/a          |
| `SystemRetentionDelete`      | ownership bypassed; state-gated   | ownership bypassed; state-gated  | ownership bypassed; state-gated | n/a          |
| Orphan catalog               | n/a                               | n/a                              | n/a                             | admin only   |

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
`RecordingSnapshotRequest` and `RecordingSnapshotResponse { revision, tasks }`; there is no
recording delta message — every recording change goes out as a `RecordingChanged` event, and the
client re-requests a filtered snapshot in response. The `revision` field is the monotonic
`QueueRevision`; clients that detect a revision gap must request a fresh filtered snapshot.

Two notifications exist for the recording subsystem:

- `RecordingChanged` (no payload) — broadcast whenever a task mutates the queue (create, edit,
  cancel, delete, finalize, retry). Triggers a filtered snapshot refresh on every subscribed
  client that holds `recording.read`.
- `RecordingRulesChanged` (no payload) — broadcast on every rule mutation (create, edit,
  delete, retain/cancel). Used by the rules view to refresh without polling.

The cancel-recording-task endpoint emits **both** events because cancelling future rule
recordings mutates the queue as well as the rule store.

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

| Situation                                     | Action                                       |
|-----------------------------------------------|----------------------------------------------|
| Materialized task, no `Scheduled` tombstone   | `AddScheduledTombstone`                      |
| `Scheduled` tombstone, no task, rule enabled  | `Materialize`                                |
| `Cancelled` tombstone, eligible inactive task | `Finalize`                                   |
| `Cancelled` tombstone, **active** task        | `ConflictingIntent` (log, manual resolution) |
| `Completed` tombstone, any task               | suppress (no rematerialize)                  |
| Task terminal, tombstone `Scheduled`          | `UpdateTombstone { kind: Completed }`        |
| Tombstone past `expires_at`                   | `PruneTombstone`                             |
| Disabled rule                                 | skip (no rematerialize)                      |
| Deleted rule                                  | leave tombstone until expiry                 |

Tombstones are retained for the longer of the EPG horizon and the 14-day minimum horizon
(`MIN_TOMBSTONE_HORIZON_SECS`). The fixed cross-store lock order is
`queue mutation boundary → rule repository mutation`.

The reconciliation pass runs at startup, before the rule scheduler's first tick, so the scheduler
never plans against half-repaired state. Two notes on how the actions are applied:

- `Materialize` cannot be executed literally — an `occurrence_key` cannot be turned back into a
  programme window. The orphan `Scheduled` tombstone is dropped instead, which lets the scheduler
  re-plan that occurrence from the rule and the EPG on its next tick.
- `ConflictingIntent` is only logged (`recording::audit`, `warn`). An active recording is never
  cancelled because of a stale intent.

Deletions interrupted by a crash are repaired in the same pass. For each task still carrying
`deleting_previous_state`, the physical file decides:

| File state                          | Action                                                                                                                                                                                    |
|-------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Gone                                | Finish the deletion — remove the task from the queue.                                                                                                                                     |
| Present, inside the recording root  | Restore the prior terminal state and clear the marker.                                                                                                                                    |
| Present, outside the recording root | Restore the prior state, leave the file alone, and log `recording_reconciliation_unsafe_path` (`recording::audit`, `warn`). Dropping the task instead would orphan a file nothing tracks. |

### 14.4 Delete with retain / cancel

`DELETE /api/v1/recording/rules/{id}?future=retain|cancel`:

- `retain` — set the rule's `enabled = false`. Existing tasks keep their `rule_id` and
  `occurrence_key` for historical provenance. The scheduler stops materializing future
  occurrences.
- `cancel` — same as `retain` plus cancel only future inactive occurrences. Active recordings
  are **never** auto-cancelled by rule deletion; the operator must resolve manually.

`cancel` touches two stores — the queue and the rule repository — and they cannot commit together.
The queue side runs first and hands back a snapshot of every occurrence it cancelled; if the rule
delete then fails, those occurrences are **restored** from that snapshot (original state and
`reserved_bytes` included) before the error is returned. So a failed `future=cancel` leaves the
rule in place *and* its upcoming recordings intact. Only if the restore itself fails does the API
report `PartialOperation { primary: "future_cancelled", secondary: "rule_delete_failed" }`, which
tells the operator exactly which side won.

## 15. At-most-once notification delivery

The notification adapter follows the at-most-once protocol:

1. The queue-mutation boundary persists a `NotificationMarker` for the lifecycle event
   (`Started` / `Completed` / `Failed`) in the same transaction as the state transition.
2. After the transaction commits, the adapter hands the notification to the **notification
   outbox** instead of delivering it inline. The recorder never blocks on, or waits for, a
   messaging provider.
3. The outbox worker persists the entry to `storage_dir/recording_notification_outbox.json`,
   then attempts delivery **per channel**.
4. A channel that fails is retried with capped exponential backoff
   (`notifications.backoff_initial_secs`, doubling, clamped to `backoff_max_secs`). A channel
   that succeeded is removed from the entry, so a retry can never deliver a duplicate to a
   channel that already got the message — this is what keeps retries compatible with
   at-most-once.
5. After `notifications.max_attempts` the entry is dead-lettered: it is dropped from the outbox
   and logged at `error` level under the `recording::audit` target with the event kind, the
   attempt count, the channels that never accepted it, and the original enqueue time.
6. On restart the outbox file is reloaded and the backlog resumes from where it stopped. A
   corrupt outbox file is logged and skipped rather than blocking startup.
7. A crash between the marker commit and the outbox write can still lose a notification. The
   window is one file write wide, and the marker prevents a duplicate on the next boot.

If the in-memory channel to the worker is full (`notifications.outbox_buffer` entries pending),
the notification falls back to a single best-effort direct send. A recording is never delayed
because a messaging provider is down.

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

## 18. Rollback

The DVR is a feature flag, so a rollback does not require a binary downgrade:

```yaml
video:
  download:
    recording:
      enabled: false
```

That stops the supervisors and the rule scheduler, answers `501 recording_disabled` on the
recording routes, serves no recording data over the WebSocket, and hides the sidebar entries.
Existing recordings and the queue are left untouched, so re-enabling resumes where you left off.

Keep the previous binary available as well: the notification outbox writes
`storage_dir/recording_notification_outbox.json`, which an older binary does not know about. It is
ignored rather than misread — an unknown file in `storage_dir` is harmless — but the queued
notifications in it are not delivered until the newer binary runs again.

## 19. Verifying the installation

After deployment or configuration changes, verify supervisor health:

```bash
# Supervisor liveness. Administrator token required.
curl -s -H "Authorization: Bearer $TOKEN" \
  http://localhost:8901/api/v1/recording/health | jq
```

A healthy install shows a non-null `reconciliation_last_run` (stamped once at boot) and a
`retention_last_tick` no older than `disk.cleanup_interval_secs`. A `null` value means that
supervisor has never completed a pass.

`bin/dvr_doctor.sh --token "$ADMIN_TOKEN"` wraps this up with the on-disk state — in particular a
`stuck_deleting` count, which should be `0` after a clean boot.

Then check the log for the two lines worth reacting to:

- `recording is enabled with no retention, no disk watermarks, and no quota` — nothing bounds
  recording disk usage. Intentional on a dedicated filesystem; a mistake otherwise.
- `enabled NewEpisode recording rule(s) cannot match` — see the limitation below.

### 19.1 Known limitation: `NewEpisode` rules

`NewEpisode` rules do not currently match anything. The scheduler matches them by walking EPG
programmes, and no EPG horizon is supplied to it yet, so only `WeeklyTimeslot` rules materialize.
The condition is logged once per process rather than failing quietly.

Until this is wired, record a recurring programme with a `WeeklyTimeslot` rule, or record
individual programmes from the EPG view.
