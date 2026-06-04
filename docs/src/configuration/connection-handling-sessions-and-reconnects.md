# Connection Handling: Sessions, HLS, Catchup and Reconnects

This page explains why Tuliprox does not treat every request as a completely new stream and why sessions are so important for streaming stability.

This matters for operators because many seemingly strange connection patterns are actually normal player behavior.

If you are changing the code rather than the configuration, also read:

- [Session Handling Internals](./connection-handling-session-implementation-notes.md)
- [Connection Handling Runtime Internals](./connection-handling-runtime-internals.md)

## Why sessions are needed at all

Many operators initially assume this model:

- a user starts one stream
- then one connection stays open until the end

In practice this is often not how players behave.

Many players behave more like this:

- several short follow-up requests
- segment changes
- reconnects
- brief overlap during seeks
- channel switching with an overlap window

Without session logic, Tuliprox would often count these operations incorrectly as "yet another new stream".

## What a session means in Tuliprox

A session is the logical playback identity of a user for one stream activity.

Important distinction:

- HLS and catchup rely heavily on logical sessions
- plain TS live playback is counted by active socket-backed stream, not by a long-lived playback session
- VOD, series, catchup, and local playback use logical sessions for reopen/seek/range continuity
- this means opening the same TS live stream twice on two sockets counts as 2 user connections when user limits are enabled
- if the user has `max_connections: 0`, user-limit admission is unlimited and those TS sockets are not rejected by the user limit

It helps Tuliprox decide:

- is this the same playback?
- should Tuliprox try to stay on the same provider account?
- may this reconnect continue the same logical connection?
- or should this be treated as a completely new start?

## HLS and why it is especially noticeable

HLS is not "one file, one connection".

Typical behavior:

- the player downloads a playlist
- then continuously fetches many small segments
- there are short idle gaps in between
- channel switches or quality changes may trigger new requests

For operators this can look like many connections in the logs. Tuliprox therefore tries to treat these follow-up requests as one logical session.

This behavior is intentional for adaptive/session-style playback.
It is not the general rule for every stream type.

## Catchup and why it can look even more unstable

Catchup and archive playback often produce:

- jumps
- backward and forward moves
- repeated requests after timeline changes

That is why catchup usually needs a longer session TTL than HLS.

## What the session TTL does

Tuliprox keeps session information for a limited time so that a short interruption does not immediately look like a completely new stream.

Important values in the reverse proxy block:

- `hls_session_ttl_secs`
- `catchup_session_ttl_secs`

In plain language:

- HLS gets a short recognition window
- catchup usually gets a somewhat longer one

## What the session TTL does not mean

It does not mean that every later socket should always be treated as the same playback.

In particular:

- HLS and catchup may reuse session identity within their configured TTL
- plain TS live playback remains socket-bound and is counted per active stream/socket
- a second TS socket therefore consumes another user connection or soft slot when user limits are enabled
- VOD, series, and local playback are not socket-bound for session tracking because seek/range/reopen requests often belong to the same playback

The TTL does not automatically mean:

- that a real provider stream always stays open the entire time

Instead, the TTL is about recognition, continuity, and provider affinity.

## Provider affinity for sessions

When a session continues, Tuliprox tries to stay on the same provider context whenever possible.

This matters because some upstream services react badly if one logical playback keeps jumping between different provider accounts or slots.

For operators this means:

- session logic is not only a comfort feature
- it also protects against unstable provider-side behavior

## What happens during a quick channel switch

During a channel switch, two things can be true at the same time:

- the old stream is not fully released yet
- the new stream is already being requested

To prevent unnecessary failures, Tuliprox uses transition tools such as grace and session reuse.

The goal is:

- ordinary switching should work
- real parallel viewing should still remain controlled

## What happens during a seek

Seek is one of the most common reasons for apparently duplicated connections.

Why?

- the player requests the new position already
- the old connection is often not fully closed yet
- for a brief moment both sides exist at the same time

Without grace and session logic, this would often end as:

- "too many connections"
- or "provider is full"

even though the user only performed a normal seek.

## User grace and session behavior

Tuliprox uses user grace as a short transition mechanism when a new request would otherwise be blocked by the user limit for a short moment.

Important for operators:

- grace is not a permanent extra entitlement
- grace is meant to bridge short transition moments
- grace is only evaluated after normal and soft admission are no longer sufficient

There are two broad modes:

- start immediately and verify later
- hold the start briefly and only continue once the decision is clear

Which one is better depends on the player and the behavior you want.

## Provider grace and session switches

Besides the user side, there is also a provider-side grace view.

This is especially important during:

- equal-priority switching
- moments where provider slots are still briefly occupied

Here Tuliprox tries not to turn the new request into an immediate failure if the old slot is likely to be released in time.

## Why HLS and catchup are still not "free"

Session reuse does not mean that HLS or catchup are ignored indefinitely.

If Tuliprox recognizes that this is genuinely a new situation, the request is counted as a new activity again.

Examples:

- a different channel
- a different session
- the session has expired
- a kick or another hard termination happened

## What happens on a kick

When a connection is deliberately kicked, not only the current stream ends.

Important for operators:

- the related session is also invalidated
- the same session should not immediately be accepted as "still valid"

This prevents a deliberately terminated stream from slipping right back through via the same session identity.

## What happens on a normal disconnect

A normal disconnect is not automatically a fault.

Depending on timing, Tuliprox may:

- keep the session briefly
- allow a resume
- or discard the session once the window expires

This is most relevant for HLS and catchup continuity.
Plain TS live playback should be understood primarily as socket-bound admission.
VOD, series, and local playback should be understood as session-based reopen/seek workflows rather than plain socket-bound playback.

## What matters when shared streams and sessions meet

Shared streams and sessions solve different problems:

- shared streams save provider slots
- sessions help with recognition and continuity

So a user can simultaneously:

- be part of a shared stream
- and still rely on session logic for HLS or catchup continuity

## Common operator questions

### "Why do I see many requests even though the user only watches one stream?"

Usually because of:

- HLS segments
- catchup jumps
- reconnects
- normal player behavior

### "Why does a session remain for a short time after the stream is gone?"

So that brief reconnects are not punished as a completely new playback.

### "Why was the same session no longer accepted after a kick?"

Because a kick is intentionally supposed to invalidate the session side as well.

## Recommendations for operators

### If HLS users often have trouble while switching

Check:

- `hls_session_ttl_secs`
- `grace_period_millis`
- `grace_period_hold_stream`

### If catchup users often fail during timeline jumps

Check:

- `catchup_session_ttl_secs`
- whether the player performs very aggressive seeks

### If users complain about "too many connections" even though they are only seeking

Check:

- admission strategies
- grace configuration
- session TTLs

## Next step

If you want to understand which user-visible symptoms result from these rules, continue with:

- [Failures and User-Visible Behavior](./connection-handling-failures-and-user-visible-behavior.md)
