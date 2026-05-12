# Connection Handling Runtime Flow

This page explains the **current runtime behavior** of Tuliprox connection handling from an operator and user-facing perspective.

It focuses on what actually happens when a stream starts, switches, seeks, reconnects, gets rejected, or replaces another stream.

If you need the code-level lifecycle and internal runtime details, continue with:

- [Connection Handling Runtime Internals](./connection-handling-runtime-internals.md)

## Why this page exists

Tuliprox connection handling can look confusing in logs and in real player behavior because several mechanisms are active at the same time:

- user limits (`max_connections`)
- optional soft slots (`soft_connections`)
- admission strategies
- provider capacity
- shared live streams
- sessions and follow-up requests
- grace behavior during switching and reconnects

This page explains the runtime from the outside: what the operator sees and why Tuliprox behaves that way.

## The short version

For a new playback request, Tuliprox roughly does this:

1. identify the logical playback
2. decide whether this is a follow-up request or a genuinely new activation
3. check user admission
4. if needed, evaluate configured admission strategies in order
5. check provider-side capacity
6. open, reuse, defer, or reject the stream
7. clean up, preserve, or expire the playback when it ends

Both the user side and the provider side must allow the playback.

## The practical runtime flow

## 1. Tuliprox first identifies the playback

Before it decides whether a stream is allowed, Tuliprox tries to determine whether the request belongs to:

- an already known playback
- or a completely new playback start

This is important because not every client behaves like "one click = one long connection".

Typical examples:

- HLS players fetch a playlist and then many small segment requests
- catchup players jump around in the timeline
- some players reopen sockets during seek or channel switching

If Tuliprox recognizes the request as the same logical playback, it can avoid treating every follow-up as a brand-new connection.

## 2. Follow-up traffic is cheaper than a new start

If Tuliprox sees that the request belongs to an already active playback, it usually treats it as a **follow-up**.

That means:

- no full admission strategy loop
- no new user-slot competition
- no unnecessary eviction logic

This is especially relevant for:

- HLS segment traffic
- catchup continuation
- validated reopen paths

## 3. If it is not a follow-up, user admission is checked

For a real new activation, Tuliprox checks user-side admission in this order:

1. normal slot
2. soft slot
3. configured admission strategies
4. deny

### Normal slot

If the user is still below `max_connections`, the stream starts normally.

### Soft slot

If the hard limit is already reached, Tuliprox may still allow the request on a soft slot if `soft_connections > 0`.

Soft slots are weaker than normal slots and are easier to preempt later.

### Admission strategies

If neither a normal nor a soft slot is available, Tuliprox evaluates the configured admission strategies.

Important:

- strategies are evaluated in the configured order
- the first matching strategy decides the result

Possible outcomes are typically:

- deny
- grace / hold briefly
- evict another stream

If the configured strategy that wins is a user-grace strategy, Tuliprox may later
re-check only the remaining strategies after that grace if the grace window expires
without the situation becoming legal in time.

## 4. Provider-side capacity is checked separately

Even if the user is allowed to stream, Tuliprox still has to check whether the upstream provider can actually serve the playback.

This means:

- the user may still be entitled to another stream
- but the request can still wait or fail
- if the provider has no free slot

Provider-side decisions include:

- immediate open
- reuse of an existing shared live stream
- wait/defer briefly
- or reject

## 5. Shared live streams can save provider slots

For live playback, Tuliprox can attach multiple viewers to the same running upstream stream.

That means:

- several clients may watch one logical live source
- but the provider only sees one upstream connection

This reduces provider-slot usage but does not automatically bypass user admission rules.

## 6. HLS, catchup, and TS do not behave the same

This is one of the most important operator-facing differences.

### HLS

HLS is session-oriented:

- playlist request
- segment requests
- reconnects during a running playback

Tuliprox therefore treats HLS as one logical playback with many follow-up requests.
Separate HLS playlist starts can still get separate session tokens, so two players behind the same IP/user-agent can watch the same channel independently.

### Catchup

Catchup is also session-oriented:

- jumps
- seeks
- repeated requests in the same playback context

### Plain TS live

Plain TS live is much stricter:

- the first request is the actual playback start
- a new parallel request is a new connection attempt
- every active TS socket is counted separately when user limits are enabled
- with `max_connections: 0`, the user-side limit does not block the same IP from opening the same or different TS streams
- it may replace another stream if admission strategies allow that
- provider capacity and shared-stream behavior still apply separately

### VOD / Series

VOD and series can use reopen and range-style behavior, but they still do not have the same manifest/segment split as HLS.
Tuliprox therefore treats follow-up range, seek, and reopen requests as the same logical playback when the session identity matches.
They are provider-affine like HLS/catchup, but they are not plain TS socket-bound playback.

## 7. What happens when the user is already at the limit

If the user is already at the hard limit, Tuliprox does **not** automatically deny the request.

It first checks:

- is a soft slot still free?
- do configured strategies allow grace or eviction?

If a configured eviction strategy matches, Tuliprox may:

1. select an eviction target
2. release the old winner
3. retry admission
4. start the new request

From the user perspective, this looks like:

- the new stream starts
- the old stream may be stopped shortly after

This is intentional replacement behavior, not an extra connection slot.

If the request was admitted by user grace first, Tuliprox does not automatically
deny when that grace later expires. It continues with only the strategies that come
after the already-used grace. If no remaining strategy can admit the request, the
result becomes final exhausted/deny.

## 8. What happens when provider capacity is full

Provider fullness is a separate problem from user fullness.

Possible visible results:

- the request waits briefly
- the request fails with a provider-capacity-style error
- a shared live stream is reused instead of opening a new upstream connection

So a user can be fully valid on the user side and still be blocked on the provider side.

Important:

- provider grace failure stays on the provider path
- it does not fall through into later user-eviction strategies

## 9. What happens when a stream ends

When playback ends, Tuliprox cleans up in one of several ways:

- remove the stream directly
- preserve it briefly for continuity
- free the counted user slot
- free the provider slot
- eventually expire the preserved state

This depends on stream type and context.

### Preserved playback

Some playback types may remain logically known for a short time after disconnect.

This is used to support:

- HLS continuity
- catchup reconnect behavior
- provider affinity over short gaps

Important:

- preserved playback is not the same thing as an active counted connection

## 10. What operators most often see in logs

Typical patterns are:

- follow-up requests on the same playback
- a new request being denied because the user is at limit
- a later eviction of the old winner if strategy order allows it
- provider-side exhaustion even though user-side admission looked fine
- preserved or session-based reconnect behavior on HLS/catchup

Without the runtime model, these logs can look random. In practice they follow fixed rules.

## 11. Background probe and resolve tasks

Tuliprox also runs background metadata and probe work.

Examples:

- ffprobe-based stream probing
- metadata resolution that includes a later probe step

Important for operators:

- these tasks do not consume normal user playback admission
- they are treated as low-priority background work
- they may be skipped, delayed, or preempted when real playback needs provider capacity

So if provider capacity is tight, Tuliprox prefers:

- real user playback first
- background probe/resolve work later

That is intentional and helps protect live operation.

## Typical visible outcomes

### Case 1: New stream starts immediately

Reason:

- user slot free
- provider slot free

### Case 2: New request replaces an old one

Reason:

- user is already at limit
- a configured `EvictUser...` strategy matches

### Case 3: Request is still denied

Reason:

- no normal slot
- no soft slot
- no matching strategy
- or provider side also cannot admit it

### Case 4: HLS or catchup survives a short interruption

Reason:

- Tuliprox treats it as the same logical playback and reuses the session context

## Practical operator recommendations

### If users complain that switching works badly

Check:

- `max_connections`
- `soft_connections`
- admission strategy order
- grace settings

### If users complain that a second stream replaces the first

Check:

- whether an `EvictUser...` strategy is configured
- whether that replacement is actually intended policy

### If users complain that the provider is full even though the user still has room

Check:

- provider max-connections
- shared live stream reuse
- provider priority / preemption settings

### If users complain about unstable HLS or catchup reconnects

Check:

- session TTLs
- preserve behavior
- grace behavior

## Related pages

- [Connection Handling](./connection-handling.md)
- [Priorities, Soft Connections and Preemption](./connection-handling-priorities-and-preemption.md)
- [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)
- [Failures and User-Visible Behavior](./connection-handling-failures-and-user-visible-behavior.md)
- [Connection Handling Runtime Internals](./connection-handling-runtime-internals.md)
