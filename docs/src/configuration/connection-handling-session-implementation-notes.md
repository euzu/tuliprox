# Session Handling Internals

This page is for maintainers working on Tuliprox session code.

If you only want the operator-facing behavior, read:

- [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)

## Why this page exists

Tuliprox uses the word "session" for several related but different mechanisms:

- user admission reuse
- logical playback identity
- socket/address tracking
- provider/account affinity
- adaptive reconnect preservation
- provider-account affinity and reservation

These mechanisms interact, but they are not the same thing.

The most important rule for future changes is:

- do not derive session-based admission from socket-binding rules
- do not derive provider/account affinity from socket-binding rules either

That exact confusion breaks VOD, series, catchup, and local reopen/seek flows.

## Core terms

### Session token

The session token is the stable logical playback identity.

It is used to answer questions like:

- is this the same playback as before?
- may this request reopen an existing playback while the user is already at the hard limit?
- should Tuliprox keep using the same provider context if possible?

### Session admission

Session admission means user-limit checks are performed with `connection_admission_for_session(...)` instead of plain `connection_admission(...)`.

This happens in `resolve_admission_with_strategies(..., use_session_admission, session_token)`.

If `use_session_admission` is `true` and the session token already exists, the request may continue even when the user is  
already at the nominal connection limit.

This is required for normal player behavior such as:

- VOD size checks
- VOD seeks
- pause/resume reopen
- HLS segment fetches
- local media reopen

### Socket-bound session

Socket binding only controls how Tuliprox tracks request addresses for the same session.

It does not decide:

- whether the request should use session admission
- whether the logical playback must stay on the same provider account

Current socket-binding policy:

- `PlaylistItemType::Live`
- `PlaylistItemType::LiveUnknown`

are socket-bound.

Everything else is not socket-bound:

- `LiveHls`
- `LiveDash`
- `Video`
- `Series`
- `Catchup`
- local playback types

### Provider/account affinity

Provider/account affinity answers a different question:

- when several requests belong to the same logical playback, must they keep using the same provider account?

The intended behavior is:

- TS-style live playback is socket-bound, but not provider-account-bound across requests
- VOD/movie/series playback is not socket-bound, but is provider/account-bound for the logical stream
- HLS/DASH playback is not socket-bound, but is provider/account-bound for the logical stream

Why:

- TS-style live playback has no seek/range workflow and each new request can be treated independently
- VOD uses range and seek requests that still belong to one playback
- HLS fetches many chunk requests that still belong to one playback

This means the same logical HLS or VOD stream must keep the same provider account for its follow-up requests, even though  
those requests may arrive on different sockets.

### Adaptive preserved session

Adaptive live playback (`LiveHls`, `LiveDash`) can stay logically alive for a short TTL even after the current socket disconnects.

That logic lives in `ActiveUserManager` and uses:

- `should_preserve_session_stream(...)`
- `build_preserved_stream_expiry(...)`
- `process_due_adaptive_expiry_entries(...)`

### Provider reservation

Provider reservation is a provider-slot affinity mechanism, not a user admission mechanism.

Current TTL mapping:

- HLS/DASH use `hls_session_ttl_secs`
- Catchup uses `catchup_session_ttl_secs`
- other item types use `0`

This affects provider reuse between short request gaps, not whether the session should use session admission.

Important:

- VOD/movie/series still have strict provider affinity on follow-up requests
- they just do not currently get an extra post-request reservation TTL like HLS/Catchup do

## Where session tokens come from

### TS live, VOD, series

The regular M3U and Xtream playback endpoints build a stable playback token with:

- `create_session_fingerprint(fingerprint, username, virtual_id)`

### Catchup

Catchup uses a different token namespace:

- `create_catchup_session_key(fingerprint, username, virtual_id)`

### HLS segment requests

Rewritten HLS URLs can carry the session token inside the encrypted segment token.

On segment fetch:

- Tuliprox decodes the HLS token
- extracts the session token if present
- falls back to `create_session_fingerprint(...)` if needed

### Local playback

Local playback uses a stable `playback_session_token` passed into `local_stream_response(...)`.

## The three decisions that must stay separate

### 1. Should this request use session admission?

For playback endpoints, the answer is yes.

That means these paths should call `resolve_admission_with_strategies(..., true, Some(session_token))`:

- M3U playback
- Xtream playback
- HLS session/segment handling
- local playback reopen logic

Reason:

- a second request for the same logical playback must be admitted as the same playback
- not as a brand-new connection

### 2. Should this session be bound to exactly one current socket?

This is controlled by `PlaylistItemType::uses_socket_bound_session()`.

Reason:

- plain TS-style live playback behaves like one active transport socket
- VOD, HLS, DASH, catchup, and local playback often use multiple short-lived sockets for one logical playback

Do not replace decision 1 with decision 2.

If you do that:

- existing VOD sessions stop reopening correctly at `max_connections: 1`
- seek/size-check flows start failing with `UserConnectionsExhausted`

### 3. Should this logical playback stay on the same provider account?

This is separate from both admission and socket tracking.

This policy is controlled by `PlaylistItemType::requires_provider_affinity()`.

Intended model:

- TS live: no provider/account pinning across requests
- VOD/movie/series: provider/account pinned across the logical stream
- HLS/DASH: provider/account pinned across the logical stream
- Catchup: provider/account pinned across the logical stream

If you collapse this into socket-binding, you get the wrong matrix:

- TS would be over-pinned
- VOD/HLS would be under-pinned

## Current code flow

### 1. Admission

Main entry point:

- `api_utils::resolve_admission_with_strategies(...)`

If `use_session_admission` is `true`, Tuliprox checks:

- `AppState::get_connection_admission_for_session(...)`
- `ActiveUserManager::connection_admission_for_session(...)`

If `false`, it falls back to:

- `AppState::get_connection_admission(...)`
- `ActiveUserManager::connection_admission(...)`

### 2. Session creation or refresh

Main entry point:

- `ActiveUserManager::create_user_session(...)`

This stores:

- session token
- virtual id
- provider
- stream url
- current address
- `socket_bound`
- `active_addrs`
- connection permission and kind

### 3. Stream registration

Main entry point:

- `ActiveUserManager::update_connection(...)`

This creates or reuses the tracked logical stream entry.

Important behavior:

- same `session_token` reuses the logical stream
- stream metrics and duration stay tied to the logical session
- for non-socket-bound sessions, the session remembers multiple active addresses

### 4. Lightweight HTTP activity

Main entry point:

- `ActiveUserManager::touch_http_activity(...)`

This is used for session continuity without creating a new logical stream, especially for HLS follow-up requests.

### 5. Seek / forced provider reopen

Main entry point:

- `api_utils::force_provider_stream_response(...)`

This path:

- releases the current provider-side connection for the session
- reacquires a provider connection
- moves the session to the request address with `update_session_addr(...)`

For non-socket-bound sessions, `update_session_addr(...)` does not forget the older active session addresses.

That is what allows a VOD session to survive:

- overlapping size-check requests
- seek requests
- overlapping teardown/start windows

The address move itself is not the provider-affinity rule.

Provider-affinity is the separate invariant that follow-up requests for the same VOD/HLS playback should continue on the same provider account.

### 6. Disconnect handling

Main entry points:

- `ActiveUserManager::release_stream(...)`
- `ActiveUserManager::release_connection(...)`

For socket-bound sessions, closing the current address removes the old address from the session immediately.

For non-socket-bound sessions:

- the closed address is removed from `active_addrs`
- if another active address still belongs to that session, the session and logical stream fall back to that address

This is the key behavior that keeps VOD and local playback stable across multiple sockets.

## URL persistence rules

Session URL handling is intentionally different by stream type.

For:

- `Video`
- `Series`
- `Catchup`
- local VOD/series playback

Tuliprox stores the canonical request URL in the session, not an ephemeral redirect target.

Reason:

- provider-side redirects can resolve a request to a non-canonical final URL
- reusing that final URL later can change or break VOD seek/reopen behavior

For live playback, following the redirected provider URL is still acceptable and often desirable.

## File map for future changes

Read these files together before changing session logic:

- `backend/src/api/api_utils.rs`
- `backend/src/api/endpoints/m3u_api.rs`
- `backend/src/api/endpoints/xtream_api.rs`
- `backend/src/api/endpoints/hls_api.rs`
- `backend/src/api/model/active_user_manager.rs`
- `backend/src/api/model/active_provider_manager.rs`
- `shared/src/model/playlist.rs`

## Tests that should stay green

These tests cover the most fragile parts of the current logic:

- `resolve_admission_with_strategies_allows_existing_session_even_when_user_is_at_limit`
- `local_stream_response_reuses_stable_playback_session_token_across_reopens`
- `vod_session_survives_overlapping_and_seek_sockets`
- `update_session_addr_prunes_previous_registration_for_socket_bound_session`
- `test_adaptive_session_release_connection_preserves_logical_stream_and_start_time`

If you change session code and one of these assumptions no longer holds, update the documentation and the tests in the same change.
