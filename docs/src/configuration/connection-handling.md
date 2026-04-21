# Connection Handling

This handbook explains how Tuliprox decides whether a stream is started, reused, briefly tolerated, preempted, or rejected.

It is written for operators rather than developers. The goal is to explain:

- what Tuliprox does in everyday operation
- why a user can sometimes start streaming immediately and sometimes cannot
- which rules apply to priorities, sessions, shared streams, and provider limits
- which visible effects operators and end users will notice

If you want to go deeper into individual topics, continue with:

- [Connection Handling Runtime Flow](./connection-handling-runtime-flow.md)
- [Priorities, Soft Connections and Preemption](./connection-handling-priorities-and-preemption.md)
- [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)
- [Connection Handling Runtime Internals](./connection-handling-runtime-internals.md)
- [Failures and User-Visible Behavior](./connection-handling-failures-and-user-visible-behavior.md)

## Why this matters

Every stream request in Tuliprox is evaluated against several limits at the same time:

- user connection limits
- soft connections
- provider capacity
- session reuse
- shared streams
- priorities
- grace behavior during switching and seeking

Because of that, a stream can fail or be terminated later for very different reasons. Without a clear model,  
the behavior can look random. In reality it follows fixed rules.

## The most important terms in plain language

### User limit

This is the normal number of simultaneous streams a user is allowed to have.

Example:

- `max_connections: 2`
- The user may normally watch 2 streams in parallel.

### Soft connection

A soft connection is an extra stream above the normal user limit.

Important:

- It is a bonus, not a guaranteed right.
- It can be preempted more easily than a normal connection.

### Priority

Priority decides which stream is protected when there is a conflict.

Important:

- lower numbers mean higher priority
- `0` is stronger than `5`
- `-10` is stronger than `0`

### Provider capacity

Tuliprox does not only enforce user limits. It also respects the actual free slots available from the upstream provider.

That means:

- a user may still be allowed to open another stream on the user side
- but the request can still be blocked
- if the provider currently has no capacity left

### Shared stream

Multiple viewers can reuse the same running live stream.

In that case:

- for the provider this is only one shared upstream connection
- for viewers, more than one client is attached to the same source

### Session

A session is Tuliprox's way of recognizing that several requests belong to the same logical playback.

This matters especially for:

- HLS
- catchup

It does not apply equally to every stream type:

- HLS and catchup are session-oriented because many follow-up HTTP requests still belong to one logical playback
- regular TS/VOD/local playback is socket-bound for admission and connection counting
- for TS/VOD, a second socket is a second connection even if it is the same user, same IP, and same stream
- quick reconnects
- channel switches with a brief overlap

### Grace

Grace is a short, controlled transition window.

It exists so a new request does not fail immediately just because:

- the old stream is still shutting down
- a player sends overlapping requests during a seek
- a provider slot has not been released yet during a switch

## The simple overall flow

When a user starts a stream, Tuliprox roughly thinks in this order:

1. Is this an existing session or a truly new start?
2. Does the user still have a free normal connection?
3. If not, is a soft connection still available?
4. If not, do configured admission strategies or grace rules apply?
5. If provider capacity is missing, can a shared stream be reused or can a lower-priority connection be preempted?
6. If no legal solution exists, the request is rejected.

This is the short version. In practice, user rules and provider rules run in parallel:

- user side: "Is this user allowed to have another stream?"
- provider side: "Is there actually a free upstream connection for it?"

Both sides must succeed.

## What is checked first

### 1. Same session or new stream?

Tuliprox first tries to recognize whether this is the same logical playback.

This matters because many players do not use "one long connection", but many short follow-up requests.

Examples:

- HLS constantly fetches new segments
- catchup performs frequent jumps
- some players briefly overlap old and new requests during seeks or channel switches

If Tuliprox recognizes the same playback, not every follow-up request is treated like a completely new stream.

This recognition is mainly relevant for HLS and catchup style playback.
Regular TS/VOD playback is intentionally stricter:

- one active socket-backed stream counts as one connection
- a parallel reopen on another socket counts as another connection
- if `max_connections` is already exhausted, only a soft slot, admission strategy, or rejection remains

### 2. Normal connection

If the user is still below the normal limit, the stream is admitted as a normal connection.

This is the preferred case.

### 3. Soft connection

If the normal user limit is already reached, Tuliprox checks whether a soft connection is still allowed.

Soft connections are intentionally weaker. They help with short-term overbooking in quiet periods, but they are the first candidates for later preemption.

### 4. Admission strategies and user grace

If neither a normal nor a soft connection is possible, Tuliprox can still evaluate configured admission strategies.

Examples:

- `GraceInstantStream`
- `GraceHoldStream`
- `EvictUserSameIpOldest`
- `EvictUserSameIpLatest`
- `EvictUserOldest`
- `EvictUserLatest`

These rules are only evaluated after ordinary user admission is already exhausted.

### 5. Provider side

Even if the user is still allowed to stream, the upstream provider must also have room.

Tuliprox then checks:

- is there still a free provider slot?
- can an already running shared stream be reused?
- may a lower-priority connection be preempted?
- can a short provider grace transition help?

## The two major protection layers

### User rules

These rules protect the operator-defined usage limits:

- normal connections
- soft connections
- sessions
- admission strategies

### Provider rules

These rules protect the real upstream:

- free provider slots
- shared streams
- priorities
- preemption
- provider grace during switch moments

A request must pass both layers.

## Typical everyday situations

### Case 1: A user simply starts another channel

- If there is still room in both the user limit and provider capacity, the stream starts normally.

### Case 2: A user reaches the limit

- If soft connections are allowed, a soft connection may still be created.
- If that is no longer possible either, only admission strategies or rejection remain.

### Case 3: A user performs a seek

- The player may briefly request the old and the new position in parallel.
- Tuliprox tries not to treat this as abuse immediately.
- This is where grace and session recognition matter.

### Case 4: Many users share the same live stream

- On the provider side this counts as one shared upstream connection.
- If preemption happens later, this shared connection can be affected as a whole.

### Case 5: An important user needs a slot

- If no provider capacity is free, Tuliprox may preempt lower-priority streams.
- This also applies to shared streams.

## Important operator rules

### Equal priority does not preempt

If two requests are equally important, Tuliprox does not simply sacrifice the existing stream.

### Higher priority wins

If a genuinely more important request arrives while no provider capacity is free, a less important stream may be preempted.

### Soft is weaker than normal

Normal connections are protected more strongly than soft connections.

### Shared streams are not a magic shield

A shared stream saves provider slots, but it is not automatically untouchable.

If it is the best valid victim, it can be terminated completely.

### A kick ends both the stream and its session

When a connection is deliberately kicked, Tuliprox should not immediately treat the same session as still valid.

### Recent eviction does not create a full user cooldown

Tuliprox now distinguishes between:

- a normal reconnect that fits into a free hard slot or soft slot
- an immediate reconnect of the just-evicted playback that would only work by kicking another stream again

Only the second case is temporarily suppressed.

Important:

- for HLS/catchup, this protection is scoped to the same session identity where Tuliprox has a stable session token
- for socket-bound TS/VOD/local playback, Tuliprox uses a short same-user, same-IP, same-channel winner protection instead
- switching to a different channel is not blocked by it
- soft admission can still succeed if a soft slot is free
- the goal is to stop endless eviction ping-pong between aggressive player reconnect loops

### Admission strategy order is semantic

Admission strategies are not a set. They are processed from top to bottom.

That means broader user-wide eviction rules can shadow narrower same-IP rules when they use the same oldest/latest policy.

Recommended:

- `evict_user_same_ip_oldest` before `evict_user_oldest`
- `evict_user_same_ip_latest` before `evict_user_latest`

## Operator checklist

If connection handling looks "strange" for your users, check these first:

- How high is `max_connections`?
- Are `soft_connections` configured?
- What `priority` does the affected user have?
- Are `admission_strategies` configured?
- Is `grace_period_millis` set sensibly?
- Are `hls_session_ttl_secs` and `catchup_session_ttl_secs` appropriate for the player in use?
- Is `share_live_streams` enabled and are multiple users watching the same channel?
- Is the actual provider already at capacity?

## Recommended reading order

If you want to fully understand the system, read these next:

1. [Priorities, Soft Connections and Preemption](./connection-handling-priorities-and-preemption.md)
2. [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)
3. [Failures and User-Visible Behavior](./connection-handling-failures-and-user-visible-behavior.md)
