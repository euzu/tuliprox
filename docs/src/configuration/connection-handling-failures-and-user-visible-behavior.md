# Connection Handling: Failures and User-Visible Behavior

This page explains what operators and end users will see when connection handling rules take effect.

The most important point:

Not every "problem" is a defect. Very often Tuliprox is reacting exactly as configured:

- a stream is intentionally rejected
- a soft connection is intentionally preempted
- a shared stream is intentionally terminated as a whole
- a request intentionally waits in a short grace phase

## The most common visible outcomes

### 1. The user gets "too many connections"

This usually means:

- from the user-rule perspective, no further legal stream can be admitted

Possible causes:

- `max_connections` is reached
- `soft_connections` are not available or already consumed
- admission strategies could not help
- an earlier grace is still active
- a session is still counted

What to check:

- `max_connections`
- `soft_connections`
- `admission_strategies`
- session TTLs
- grace settings

### 2. The user gets provider-exhausted behavior

This means:

- the user might still be allowed to stream on the user side
- but the provider currently has no free slot left

Possible causes:

- real provider exhaustion
- no reusable shared stream was available
- no valid preemption target existed
- equal priority prevented preemption

What to check:

- provider capacity
- priorities of the affected users
- whether shared streams are in use
- whether a higher-priority user currently has precedence

### 3. The user sees `low_priority_preempted`

This means:

- the stream started legally
- but later a more important request arrived
- and the current stream became the valid victim

Important:

- this is not a random interruption
- it is an intentional result of the priority model

If a shared stream was affected, several viewers may see the same result at once.

### 4. A stream starts with a short delay

This is often not a bug, but one of these behaviors:

- `GraceHoldStream`
- provider-side grace
- intentional holding so Tuliprox does not make a wrong decision too early

In plain language:

- Tuliprox is waiting briefly to see whether the transition resolves cleanly

### 5. A stream starts first and is terminated later

This can happen when:

- a grace phase allows the start temporarily
- but the situation does not become legal in time

Then the behavior is:

- the start was provisionally allowed
- the later final decision was negative

On current runtime builds, a user-admission grace failure may also trigger one more
step before the final negative result:

- Tuliprox evaluates only the remaining configured strategies after the already-used grace
- a later eviction strategy may still rescue the request
- if that also fails, the result becomes final exhausted

### 6. A kick feels "harder" than a normal disconnect

This is intentional.

With a kick:

- the stream ends
- the related session is also invalidated

That prevents the same session from immediately undoing the kick.

## Typical operator scenarios

### Scenario A: "The user says they only switched channels"

Likely causes:

- the old and new stream briefly overlapped
- grace was too short or not well suited
- the provider still looked full during the switch moment

Check:

- `grace_period_millis`
- `grace_period_hold_stream`
- priority
- provider limits

### Scenario B: "The user says they only seeked"

Likely causes:

- the seek created overlapping requests
- the session window was too short
- the user limit was very tight

Check:

- `hls_session_ttl_secs`
- `catchup_session_ttl_secs`
- grace configuration

### Scenario C: "Two users watched the same channel and then both dropped at once"

Likely cause:

- they were attached to a shared stream
- that shared stream was preempted as a whole

Check:

- the priority of the later requester
- whether the shared stream only had low-priority viewers
- whether `low_priority_preempted` fallback content is configured

### Scenario D: "The same group of users sometimes switches successfully and sometimes not"

Likely causes:

- sometimes a shared stream is reused and sometimes not
- sometimes a slot is freed in time and sometimes not
- equal priority blocks preemption
- the session is sometimes still valid and sometimes already expired

Check:

- whether it was truly the same channel or a different source
- session TTLs
- provider load
- shared-stream usage

## How to classify failures correctly

### First ask: user side or provider side?

User side:

- user limits
- soft connections
- admission strategies
- grace
- sessions

Important:

- user-grace failure may still fall through to later remaining strategies
- provider-grace failure does not

Provider side:

- no free slots
- priorities
- shared streams
- preemption

Many incidents become easier to understand once you separate those two layers.

### Then ask: immediate or delayed?

Immediate:

- admission or provider allocation was not possible right away

Delayed:

- grace expired
- a higher-priority request arrived later
- a shared stream was preempted later

## What operators often misread

### "If something ended later, it must have been a network issue"

Not necessarily. It can just as easily have been a later priority or grace decision.

### "If several users are affected at once, Tuliprox must be unstable"

Not necessarily. During shared-stream preemption that is exactly the expected behavior.

### "If a user was allowed to start, they should never be stopped later"

Not necessarily. Soft connections and low-priority streams can be sacrificed later.

## Operational recommendations

### If you want as few complaints as possible

- use priorities deliberately and sparingly
- use soft connections only where preemption is acceptable
- tune session TTLs for the actual player types in use
- do not set grace too aggressively low

### If you want hard precedence rules

- give critical users clearly stronger priority values
- do not rely on "it will probably distribute fairly"

### If you rely heavily on shared streams

- document internally that one shared upstream connection can affect all attached viewers together

## Quick incident checklist

When an operator wants to classify an incident, these questions are often enough:

1. Was it the same session or a truly new start?
2. Was the user already at the normal limit?
3. Was any soft capacity still available?
4. Was the provider full?
5. Were multiple viewers attached to a shared stream?
6. Did someone with higher priority arrive later?
7. Was a grace phase active?

## Further reading

- [Connection Handling Overview](./connection-handling.md)
- [Priorities, Soft Connections and Preemption](./connection-handling-priorities-and-preemption.md)
- [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)
