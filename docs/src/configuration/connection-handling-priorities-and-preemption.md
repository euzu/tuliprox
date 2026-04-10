# Connection Handling: Priorities, Soft Connections and Preemption

This page explains how Tuliprox decides which stream may stay alive when no provider capacity is left.

This is usually the hardest part for operators to reason about because several rules interact here:

- normal connections
- soft connections
- priorities
- shared streams
- real provider limits

## The most important rule first

Tuliprox does not preempt at random. It follows fixed precedence rules.

The general idea is:

- important streams should remain stable
- less important streams may be sacrificed
- shared streams save capacity, but they are not automatically protected

## How priority is read

In Tuliprox:

- lower numbers mean higher priority

Examples:

- `-10` is stronger than `0`
- `0` is stronger than `5`
- `5` is stronger than `20`

Many operators intuitively expect the opposite. In Tuliprox, the smaller number is the stronger priority.

## Two types of connections

### Normal connection

This is a stream within the user's normal limit.

Normal connections are the stronger class.

### Soft connection

This is an extra stream above `max_connections` when `soft_connections` are enabled.

Soft connections are the weaker class. They are intentionally easier to preempt.

## Who may preempt whom

### Normal against soft

A normal request may preempt an existing soft connection.

It does not need a better numeric priority value for that. The difference between `Normal` and `Soft` is already enough.

In plain language:

- a real normal slot is always stronger than a bonus slot

### Soft against normal

A soft request may not preempt a normal connection.

In plain language:

- a bonus slot may not push out a real standard slot

### Normal against normal

Here priority decides.

Rule:

- only if the new request truly has higher priority
- equal priority does not preempt

### Soft against soft

Here priority also decides.

Rule:

- only a soft request with higher priority may preempt another soft connection

## What happens with equal priority

Equal priority does not preempt.

This is intentional so the behavior remains stable and predictable.

Example:

- User A has priority `0`
- User B has priority `0`
- The provider is full
- User B starts a new stream

Result:

- User B may not simply throw out A

Instead, Tuliprox needs:

- a free slot
- a shared-stream reuse case
- a grace-based solution
- or the request is rejected

## What happens in an exact tie

If several possible victims share the same weak priority, Tuliprox chooses the oldest victim first.

In plain language:

- the longest-running stream in that weakest group is removed first

This avoids unpredictable switching between candidates.

## How shared streams fit into these rules

A shared stream is only one upstream connection from the provider's perspective.

That means:

- 2, 5, or 20 viewers may be attached to the same live stream
- on the provider side this still consumes just one shared slot

## Which priority a shared stream has

A shared stream does not simply keep "the priority of the first viewer".

Instead, Tuliprox derives an effective shared priority from the active viewers.

Simplified:

- within the currently effective connection class, Tuliprox uses the strongest remaining subscriber priority
- if at least one subscriber is `Normal`, the shared stream is treated as `Normal`
- only if all subscribers are `Soft`, the shared stream is treated as `Soft`

What this means for operators:

- a later high-priority subscriber can make the shared stream harder to preempt
- if that important subscriber leaves, the shared stream can become weaker again

## Can a shared stream be preempted completely?

Yes.

This is important:

- if no provider capacity is free
- and a shared stream is the best valid victim
- Tuliprox does not remove only one viewer
- it terminates the shared provider connection as a whole

Consequence:

- all viewers on that shared stream lose their upstream at the same time

If a `low_priority_preempted` fallback output is configured, those viewers may see the fallback content instead of a hard disconnect.

## Example: several low-priority users share the same stream

Situation:

- several users with low priority share channel A
- an important user wants channel B
- no free provider connection is available

Result:

- if channel A and channel B compete for the same provider lineup
- and the new user really has higher priority
- the shared stream for A may be preempted as a whole

Then the following happens:

- the shared provider connection is released
- the new important stream receives the slot
- all viewers on the old shared stream are affected at once

## Example: an important user joins the same already shared channel

Situation:

- low-priority users are already sharing channel A
- an important user also starts channel A

Result:

- in this case Tuliprox often does not need a new provider connection
- the user can join the existing shared stream
- therefore no preemption may be necessary at all

## Provider grace during switching

There is not only user grace, but also provider-side transition handling.

This matters in situations such as:

- a quick channel switch
- the old stream is just about to close
- the new stream has equal importance
- the provider appears full for a very short moment

In those cases Tuliprox does not always reject immediately. It may wait briefly to see whether the old slot is released in time.

This is especially important so equal-priority switch operations do not fail unnecessarily.

## What soft connections are good for

Soft connections are useful when you want to give operators or premium users more flexibility without weakening the overall protection model too much.

They are useful for:

- short-term overbooking
- comfort during quiet periods
- bonus slots for users who do not need a hard guarantee

They are not suitable for:

- hard guarantees
- critical primary streams
- user groups that must never be preempted

## Practical operator rules

### If important users should remain stable

- give them a smaller priority number
- do not rely on soft connections for critical streams

### If many equal users should be treated fairly

- give them the same priority
- then they do not preempt each other

### If shared streams are used heavily

- remember that shared streams save provider slots
- but they can still be preempted as a whole in a conflict

### If soft connections trigger complaints

That is often not a bug, but the intended design:

- soft is supposed to help
- but soft is not supposed to be as strong as normal

## Common misunderstandings

### "More viewers on a shared stream should make it safer"

No. The decisive factors are not the number of viewers, but:

- the connection class
- the effective priority
- provider scarcity

### "Equal priority means the newer request wins"

No. Equal priority does not preempt.

### "Soft is just another fully normal slot"

No. Soft is intentionally weak and remains preemptable.

## Next step

If you want to understand why many players create multiple requests for what users perceive as a single stream, read:

- [Sessions, HLS, Catchup and Reconnects](./connection-handling-sessions-and-reconnects.md)
