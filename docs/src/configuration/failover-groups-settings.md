# Failover Groups: Settings Reference

This page explains every failover group setting: what it does, which values it takes, its default, when to change it,
and an example. For how failover groups work as a whole, see [Failover Groups & Stream Failover](./failover-groups.md).

## Where the settings live

| Where                                                          | What                                                                 |
|:---------------------------------------------------------------|:---------------------------------------------------------------------|
| `config.yml` → `failover_groups`                               | All settings below. They apply to every target with failover groups. |
| Web UI: **Settings → Failover groups**                         | Edits the same block.                                                |
| `source.yml` → `targets[].options.failover_groups`             | `enabled` and a few [overrides](#target-overrides) for one target.   |
| Web UI: **Source editor → Targets → target → Failover groups** | Edits the same target block.                                         |

Nothing happens until a target sets `enabled: true`. The global block is optional; without it every setting has
its default.

A complete global block with all defaults:

```yaml
failover_groups:
  match_by: caption
  match_as_ascii: false
  match_normalize:
    strip_regex: null
    ignore_tokens: []
    aliases: {}
  epg_conflict: split
  name_strip_quality: false
  policy: auto
  allow_provider_grace: true
  start:
    timeout_secs: 15
    max_attempts: 4
  failover:
    enabled: true
    no_data_timeout_secs: 8
    slow_source_pct: 90
    slow_source_secs: 15
    max_outage_secs: 20
    same_source_retries: 1
    stable_after_secs: 30
    max_switches: 6
    max_switches_window_secs: 600
    audio_language: prefer
  circuit:
    source_failures: 2
    cooldown_secs: 60
    cooldown_max_secs: 900
    input_failures: 3
    input_window_secs: 120
```

---

## Grouping

Grouping decides which channels become one channel. It runs during the playlist update, after the target's filters,
renames and mappings, so it sees the channel names as the target outputs them. Changes take effect with the next
playlist update.

### How two channels are found equal

For every live channel Tuliprox builds a **match key**. Channels with the same key form a group (a key with a single
channel leaves that channel alone). The key is built in this order:

1. Take the field chosen by [`match_by`](#match_by).
2. Remove everything [`strip_regex`](#match_normalizestrip_regex) matches.
3. With [`match_as_ascii`](#match_as_ascii), turn accented and special letters into plain letters.
4. Split into words at every character that is not a letter or digit (spaces, `:`, `|`, `-`, brackets, ...).
5. Drop quality words (`SD`, `HD`, `FHD`, `UHD`, `QHD`, `4K`, `480p`, `576p`, `720p`, `1080p`, `1440p`, `2160p`) and
   the words of [`ignore_tokens`](#match_normalizeignore_tokens).
6. Join the rest in lower case with single spaces.
7. Replace the key if [`aliases`](#match_normalizealiases) maps it to another key.

Example with `strip_regex: "^[A-Z]{2}:\\s*"`, `ignore_tokens: [backup]` and `aliases: {"das erste": "ard"}`:

| Channel name        | After step 2    | Words kept (step 5) | Key after step 6 | Final key |
|:--------------------|:----------------|:--------------------|:-----------------|:----------|
| `DE: Das Erste HD`  | `Das Erste HD`  | `Das`, `Erste`      | `das erste`      | `ard`     |
| `ARD [FHD]`         | `ARD [FHD]`     | `ARD`               | `ard`            | `ard`     |
| `DE: ARD HD backup` | `ARD HD backup` | `ARD`               | `ard`            | `ard`     |
| `ZDF-HD`            | `ZDF-HD`        | `ZDF`               | `zdf`            | `zdf`     |

The first three channels become one group `ard`. `ZDF-HD` stays a normal channel unless another input has a ZDF
channel too.

### `match_by`

Which channel field the match key is built from.

| Value            | Uses                                                                                                                   |
|:-----------------|:-----------------------------------------------------------------------------------------------------------------------|
| `caption`        | The title, or the name when the title is empty. Right for most setups.                                                 |
| `name`           | The `name` field only.                                                                                                 |
| `title`          | The `title` field only.                                                                                                |
| `epg_channel_id` | The EPG channel id, compared without case. Steps 2 to 7 above do not apply. Channels without EPG id are never grouped. |

**Default:** `caption`. **Can be overridden per target.**

Use `epg_channel_id` when your providers name channels differently but carry clean, identical EPG ids – for example
`ZDF HD` at one provider and `ZDF Deutschland` at the other, both with `ZDF.de`.

```yaml
match_by: epg_channel_id
```

### `match_as_ascii`

Compares names after turning accented and special letters into plain letters: `Télé 5` and `Tele 5` match, as do
`Ö3` and `O3`.

**Default:** `false`. **Can be overridden per target.**

Turn it on when one provider writes names with accents and another without.

### `match_normalize.strip_regex`

A [regular expression](https://docs.rs/regex/latest/regex/#syntax); every match is removed from the name before the
key is built. Use it for prefixes and suffixes that differ between providers.

**Default:** none.

| Goal                                | `strip_regex`             | Name               | Becomes   |
|:------------------------------------|:--------------------------|:-------------------|:----------|
| Drop a country prefix like `DE:`    | `"^[A-Z]{2}:\\s*"`        | `DE: ZDF HD`       | `ZDF HD`  |
| Drop a prefix like `DE -` or `DE /` | `"^[A-Z]{2}\\s*[-/]\\s*"` | `DE - ZDF HD`      | `ZDF HD`  |
| Drop anything in round brackets     | `"\\(.*?\\)"`             | `ZDF (Germany) HD` | `ZDF  HD` |

For a prefix with a pipe (`DE | ZDF`) use `"^[A-Z]{2}\\s*\\|\\s*"`. Without stripping, the country code stays a word
of the key: `DE: ZDF` becomes `de zdf` and does not match `ZDF` of a provider without prefix.

In YAML, write the expression in double quotes and double every backslash (`\\s`). In the Web UI, enter it once
(`^[A-Z]{2}:\s*`).

### `match_normalize.ignore_tokens`

Words that are dropped when names are compared, in addition to the quality words. Case does not matter. A token is a
whole word: `backup` drops `ZDF backup` → `zdf`, but not `ZDFbackup`.

**Default:** empty.

```yaml
match_normalize:
  ignore_tokens: [backup, hevc, h265, raw, "+1"]
```

Careful with words that tell channels apart: ignoring `plus` would join `Sky Sport` and `Sky Sport Plus`.

### `match_normalize.aliases`

Maps one match key to another, for channels that have different names at different providers. Both sides are
normalized like a channel name first, so write them as you would read them: `"Das Erste"` and `"das erste"` are the
same.

**Default:** empty.

```yaml
match_normalize:
  aliases:
    "das erste": "ard"
    "zdf deutschland": "zdf"
    "rtl television": "rtl"
```

In the Web UI, enter the name on the left and the name it should be compared as on the right.

### `epg_conflict`

What happens when channels with the same key carry **different** EPG ids – often a sign that they are different
channels with the same name, for example two regional `WDR` channels.

| Value    | Effect                                                                                                                                       |
|:---------|:---------------------------------------------------------------------------------------------------------------------------------------------|
| `split`  | One group per EPG id. Channels without EPG id in such a key stay normal channels. A split part with a single channel stays a normal channel. |
| `ignore` | All channels with the key form one group, whatever their EPG ids.                                                                            |

**Default:** `split`. **Can be overridden per target.**

Example: `WDR HD` (EPG `wdr.koeln`), `WDR FHD` (EPG `wdr.koeln`) and `WDR` (EPG `wdr.dortmund`). With `split`,
the two Köln channels form a group and the Dortmund channel stays alone. With `ignore`, all three are one group.

Use `ignore` only when your providers use different EPG ids for the same channel and you are sure the names are
unique.

### `name_strip_quality`

Removes quality words from the name of the joined channel, so the group is shown as `ZDF` instead of `ZDF HD`. Only
whole, space-separated words are removed: `ZDF [FHD]` becomes `ZDF`, `ZDF-HD` stays `ZDF-HD`.

**Default:** `false`. **Can be overridden per target.**

The group takes name, logo and EPG id of its first channel; this setting changes only the name.

### `policy`

The order in which the channels of a group ("members") are tried.

| Value      | Order                                                                                                         |
|:-----------|:--------------------------------------------------------------------------------------------------------------|
| `auto`     | By score: 60 % reliability history, 30 % picture quality, 10 % position in the playlist. Paused members last. |
| `priority` | By position in the playlist (or the order in the overrides file). Paused members last.                        |

**Default:** `auto`. **Can be overridden per target.**

- **Reliability history** comes from the stream history: how often a member started and played without problems in
  the last 24 hours and 7 days. It needs `reverse_proxy.stream_history` and `reverse_proxy.qos_aggregation` in
  `config.yml`; without them every member counts as average.
- **Picture quality** comes from the metadata prober (resolution and bitrate); without a probe the quality word in
  the name is used (`FHD` beats `HD`).
- **Position:** the channel that appears first in the target playlist ranks highest.

Use `priority` when you want a fixed order, for example "always the paid provider first, the second provider only
as backup". Sort your playlist or the overrides file accordingly.

---

## Start

These settings control what happens when a client starts a group. They apply to the next stream start.

### `allow_provider_grace`

Providers can have a grace connection: one connection above `max_connections` that is allowed for a short time while
an old stream is closing. With this setting a start may use it when no regular connection is free.

**Default:** `true`. **Can be overridden per target.**

Switches while watching never use the grace connection. Turn it off for targets with many users on few provider
connections, so a group start does not hold the slot other users need while they zap.

### `start.timeout_secs`

The longest time a start may try members – selecting, opening and checking that the provider sends a video stream.
When it runs out, the client gets the "channel unavailable" video.

**Default:** `15`. `0` means no limit.

Example: member 1 does not answer (given up after 4 seconds), member 2 answers with HTTP 403, member 3 plays after
2 seconds. The start takes about 6 seconds and stays well inside 15.

Lower it (for example to `8`) when your players give up early; raise it when your providers are slow to answer.

### `start.max_attempts`

The most members a start tries.

**Default:** `4`. Must be greater than 0.

With 6 members and `max_attempts: 4`, at most the 4 best-ranked members are tried before the start gives up.

---

## Failover while watching

When the playing member fails, Tuliprox opens the next member and continues the **same** response, so the player
does not notice more than a short freeze. This works for MPEG-TS streams (direct and shared); HLS streams change the
member only when a new stream starts. Changes apply to the next stream start.

### `failover.enabled`

Turns the switch while watching on or off. With `false` a group still starts on the best member, but the stream ends
when its member fails, as for a normal channel.

**Default:** `true`. **Can be overridden per target** (as `failover: false`).

Turn it off for a target whose players show artefacts or stop when the stream changes – some built-in TV players
do. VLC, Kodi, ffmpeg-based players and most IPTV apps handle the switch.

### `failover.no_data_timeout_secs`

A member that sends no picture and no sound for this long is replaced.

**Default:** `8`. `0` turns the check off.

Keep it above the keyframe interval of your channels: after a switch, Tuliprox waits up to 4 seconds for a keyframe
of the new member, and some sports channels send a keyframe only every 5 to 10 seconds. Too short a value replaces
members that are merely slow to start. Raise it to `12`–`15` for providers that pause for a few seconds now and then
without really failing.

### `failover.slow_source_pct` and `failover.slow_source_secs`

A member must deliver video at least as fast as it is played. A member that is slower than this percentage of
real-time speed for `slow_source_secs` seconds is replaced.

**Defaults:** `slow_source_pct: 90`, `slow_source_secs: 15`. `slow_source_pct: 0` turns the check off; at most `100`.

What the numbers mean: with `90`, the member must deliver at least 9 seconds of video every 10 seconds. A member
that delivers only 7 seconds every 10 seconds drains the player's buffer – after a while the picture stutters or
stops to buffer. If that lasts for 15 seconds, Tuliprox switches before the viewer sees it.

The speed is measured with the clock inside the stream (PCR), not by counting bytes, so channels with a low bitrate
are not mistaken for slow ones.

| Situation                                            | Suggestion                                                           |
|:-----------------------------------------------------|:---------------------------------------------------------------------|
| Switches happen although the picture is fine         | Lower `slow_source_pct` to `80` or raise `slow_source_secs` to `30`. |
| The picture stutters for a long time before a switch | Lower `slow_source_secs` to `8`.                                     |
| You do not want this check                           | `slow_source_pct: 0`.                                                |

### `failover.max_outage_secs`

The longest time without picture while Tuliprox looks for a replacement. When no member plays within this time, the
stream ends (with the "channel unavailable" video, if configured).

**Default:** `20`. Must be greater than 0.

During the search, the player receives empty packets every half second so it keeps the connection. A member that
played for 5 seconds (or half of `max_outage_secs`, if that is shorter) counts as working; a later failure starts a
new outage with the full time.

### `failover.same_source_retries`

A member that played well (for at least [`stable_after_secs`](#failoverstable_after_secs)) and then fails is first
tried again this many times before another member is used. Short hiccups of a good provider are common; reopening it
is often the best choice.

**Default:** `1`. `0` switches to another member at once.

### `failover.stable_after_secs`

After this much smooth playback a member counts as stable: the current incident is over, earlier failed members may
be tried again, and a new failure first retries the same member (see above).

**Default:** `30`.

### `failover.max_switches` and `failover.max_switches_window_secs`

The switch limit. When more than `max_switches` switches happen within `max_switches_window_secs`, the stream ends
instead of jumping between members that all fail.

**Defaults:** `max_switches: 6`, `max_switches_window_secs: 600` (10 minutes). `max_switches: 0` turns the limit off.

Keep the limit on: a provider that answers every request with a short error video would otherwise keep a stream
switching forever.

### `failover.audio_language`

Whether a switch must keep the audio language of the member that played (as found by the metadata prober).

| Value     | Effect                                                                               |
|:----------|:-------------------------------------------------------------------------------------|
| `prefer`  | Members with the same language rank higher; others are used when nothing else works. |
| `require` | Only members with the same language (or without a known language) are used.          |

**Default:** `prefer`.

Use `require` when a group mixes language versions, for example an English and a German feed of the same sports
channel.

---

## Pausing failing members (`circuit`)

A member that keeps failing is paused, so starts and switches try the others first. Pauses belong to the member
(input and stream), so they are shared by all targets. Answers like "too many connections" (HTTP 429/509) never
count as a failure – the member is fine, the account is just full.

### `circuit.source_failures`

How many failures in a row pause a member. A successful start resets the count.

**Default:** `2`. Must be greater than 0.

### `circuit.cooldown_secs` and `circuit.cooldown_max_secs`

How long a member stays paused. After the pause it is tried once; if that fails, the next pause is twice as long, up
to `cooldown_max_secs`. A success ends the pausing.

**Defaults:** `cooldown_secs: 60`, `cooldown_max_secs: 900`. A `cooldown_max_secs` below `cooldown_secs` is raised to
it.

Example with the defaults: pauses of 1, 2, 4, 8 minutes, then 15 minutes for every further failure.

A paused member is not removed: when every member of a group is paused, the best of them is still tried.

### `circuit.input_failures` and `circuit.input_window_secs`

When **several different** channels of one input fail within a short time, the provider itself is probably down. Then
the whole input is paused and its members are tried last.

**Defaults:** `input_failures: 3`, `input_window_secs: 120`. `input_failures` must be at least 2; the failures must
come from at least two different members.

Example: within 2 minutes, `ZDF` of provider A fails twice and `ARD` of provider A once – 3 failures from 2 members.
All channels of provider A now rank behind provider B until the pause ends.

---

## Target overrides

A target turns failover groups on with `enabled` and can override these settings. Every override is optional; a
missing one uses the global value. In the Web UI each override offers "Global (*value*)".

| Key                    | Overrides                                       |
|:-----------------------|:------------------------------------------------|
| `enabled`              | – (target only, default `false`)                |
| `match_by`             | [`match_by`](#match_by)                         |
| `match_as_ascii`       | [`match_as_ascii`](#match_as_ascii)             |
| `epg_conflict`         | [`epg_conflict`](#epg_conflict)                 |
| `name_strip_quality`   | [`name_strip_quality`](#name_strip_quality)     |
| `policy`               | [`policy`](#policy)                             |
| `allow_provider_grace` | [`allow_provider_grace`](#allow_provider_grace) |
| `failover`             | [`failover.enabled`](#failoverenabled)          |

`match_normalize`, `start`, the failover timing and `circuit` exist only globally: the pause state belongs to the
member and is shared by all targets, and the timing describes your providers, not a target.

---

## Recipes

### Two providers with the same channels

```yaml
# config.yml – nothing needed, the defaults fit
# source.yml
targets:
  - name: tv
    options:
      failover_groups:
        enabled: true
```

### Providers with country prefixes and different spellings

```yaml
# config.yml
failover_groups:
  match_as_ascii: true
  name_strip_quality: true
  match_normalize:
    strip_regex: "^[A-Z]{2}\\s*[:|-]\\s*"
    ignore_tokens: [backup, hevc]
    aliases:
      "das erste": "ard"
```

### A paid provider first, a cheap one only as backup

```yaml
# config.yml
failover_groups:
  policy: priority
```

Put the paid provider's input first in the source, so its channels come first in the playlist.

### One target for a TV box that cannot handle a switch

```yaml
# source.yml
targets:
  - name: living-room
    options:
      failover_groups:
        enabled: true
        failover: false
```

### Many users on few provider connections

```yaml
# source.yml
targets:
  - name: family
    options:
      failover_groups:
        enabled: true
        allow_provider_grace: false
      share_live_streams:
        mpeg_ts: true   # viewers of the same group share one provider connection
```

---

## Troubleshooting

| Problem                                      | Check                                                                                                                                                                                   |
|:---------------------------------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Channels that should be one are not grouped  | Compare their names after the target's renames; add `strip_regex`, `ignore_tokens` or `aliases`; check that the EPG ids are equal or set `epg_conflict: ignore`; run a playlist update. |
| Different channels were joined               | Keep `epg_conflict: split`; remove too broad `ignore_tokens`; use the mapping field `failover_group: ""` to keep a channel out.                                                         |
| A switch shows artefacts for a moment        | Normal when the members differ in codec or resolution; members with the same codec and resolution are preferred. Turn `failover` off for players that cannot handle it.                 |
| Streams switch although the picture was fine | Raise `no_data_timeout_secs`; lower `slow_source_pct` or raise `slow_source_secs`.                                                                                                      |
| Streams end after many switches              | All members fail; check the providers. `max_switches` ended the stream as intended.                                                                                                     |
| A good provider is always tried last         | It is paused after failures; wait for the pause to end or check the events `failover_group.*`.                                                                                          |
