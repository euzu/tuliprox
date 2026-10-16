# Channel Groups and Stream Failover

Many setups carry the same live channel from several inputs: two providers, two accounts of one provider, or an
HD and an FHD variant. **Channel groups** collapse these equal channels into **one channel** in the target. When a
client plays it, Tuliprox picks the best reachable source ("member") and, if that source fails while the client is
watching, switches to another member **underneath the open response** – the player keeps its connection and does
not have to reconnect.

Channel groups are **turned on per target** and are **off by default**. How channels are matched and how sources are
selected and switched is set **once in `config.yml`** under `channel_groups`; a target can override a few of these
settings where it needs to.

## Quick start

```yaml
# source.yml
sources:
  - inputs: [provider_a, provider_b]
    targets:
      - name: tv
        output:
          - type: xtream
        options:
          channel_groups:
            enabled: true
```

With this, live channels of `provider_a` and `provider_b` whose caption matches (quality words such as `HD`, `FHD`,
`4K` are ignored) appear once in the `tv` target. All other settings use the defaults of `config.yml`.

In the Web UI:

- **Settings → Channel groups** edits the global block of `config.yml`.
- **Source editor → Targets → *target* → Channel groups** turns the feature on for a target and holds its overrides.
  Every override offers "Global (*value*)" to keep the global setting.

## How channels are grouped

Grouping runs at the end of the target processing, after filters, renames and mappings.

1. Only live channels take part (`XtreamCluster::Live`, no local library items).
2. Each channel gets a **match key** from the field chosen by `match_by` (`caption`, `name`, `title` or
   `epg_channel_id`), normalized with `match_as_ascii` and `match_normalize`.
3. Channels with the same key form a group; a key with a single channel leaves that channel as it is.
4. With `epg_conflict: split`, a group whose members carry **different EPG ids** is split into one group per EPG id,
   so channels with different programmes are never mixed. `ignore` keeps such groups together.
5. The group appears as one playlist entry. Its name, logo and EPG id come from its first member; with
   `name_strip_quality: true` the quality words are removed from the name. The members disappear from the
   playlist but keep their stable virtual ids, so old favourites and direct member requests still play (a request
   for a member id plays the group).

The mapping DSL can steer grouping per channel with the header field `channel_group`: a value forces the channel
into the group with that key, an empty string (`""`) keeps the channel out of any group.

### Manual groups and corrections

Per target, an optional overrides file under `<config_path>/channel_groups/<target>--<hash>.yml` holds manual groups
(`groups`), corrections of automatic groups (`auto_overrides`: order, added/removed members, name, logo, EPG id,
policy) and excluded channels (`excluded`). Members are referenced by input name and provider stream id. A manual
group with fewer than two resolvable members is dropped; members that vanished from the provider are
skipped with a warning. There is no Web UI editor for this file yet.

## How a source is selected

When a client starts a group, Tuliprox ranks the members and opens the best one that has provider capacity.

- **Ranking (`policy: auto`).** Members are scored from their QoS history (24 h / 7 d success, needs
  `stream_history` and `qos_aggregation` in `config.yml`), their probed quality (resolution, codec, bitrate from the
  metadata prober) and their position in the group. Members with an open circuit or a long failure streak go last.
  `priority` keeps the configured member order and only moves failing members back.
- **Capacity.** A member with a free provider connection wins over a better member that would need the grace slot
  (`allow_provider_grace`) or the preemption of a lower-priority stream. Inputs in maintenance are skipped.
- **Start attempts.** If a member cannot be opened (HTTP error, provider refuses the account, no MPEG-TS/playlist in
  the first bytes, no answer), the next member is tried, up to `start.max_attempts` and bounded by
  `start.timeout_secs`. A provider that does not answer at all is given up after 4 seconds per member.
- **Pausing failing sources.** `circuit.source_failures` consecutive failures of one member pause it for
  `circuit.cooldown_secs`, doubling up to `circuit.cooldown_max_secs` while it keeps failing. Failures of at least two
  different members of one input within `circuit.input_window_secs` (`circuit.input_failures` in total) put the whole
  input last. This state is shared by all targets, because it belongs to the source, not to the target.
  "Too many connections" answers (`429`/`509`) never open a circuit.

HLS entries use the same start selection; with the shared HLS cache enabled, viewers of the same member share one
origin session.

## Failover while watching (MPEG-TS)

With `failover.enabled` (default on), MPEG-TS playback – direct or [shared](./source.md#326-options) – replaces a
running member when

- its provider body ends or fails (`upstream_eof`, `upstream_error`),
- it sends no media for `failover.no_data_timeout_secs` (`stall`),
- it is too slow (`underrun`): it delivers less than `failover.slow_source_pct` percent of real-time speed for
  `failover.slow_source_secs`. With 90 % a source must deliver at least 9 seconds of video every 10 seconds;
  a slower source drains the player's buffer and the picture stutters. The speed is measured from the stream's PCR.

With `failover.enabled: false` a group still starts on the best source, but the stream ends with its member.

The switch keeps the client response open. While the next member opens, null packets are sent every 500 ms so players
and proxies do not time out. The new member is spliced in at a clean point: the output continues with its PAT and
PMT (version numbers continue the previous member's, CRC recomputed) followed by a keyframe, and the first packet of
each stream carries the MPEG-TS `discontinuity_indicator`. Players such as VLC, ffmpeg-based players and Kodi handle
this well; some built-in TV players may stutter when codec or resolution change between members, so the ranking
prefers members with the same codec, resolution and audio as the one that played.

Rules of a switch:

- A member that played for at least `failover.stable_after_secs` is retried `failover.same_source_retries` times before
  others.
- Runtime switches never use the provider grace slot and never preempt other streams.
- A member whose slot was preempted by a higher-priority stream is not failed over; the stream ends as before.
- Members that refuse the account or have no capacity, or that deliver no MPEG-TS, are not asked again within the
  same switch. Others are retried in rounds with a growing pause (0.5 s up to 4 s).
- **Outage limit.** The time without playback while looking for a replacement is bounded by `failover.max_outage_secs`.
  A member counts as playing once it has sent media for 5 seconds (or half of `max_outage_secs`, if that is shorter);
  then a later failure starts a fresh outage limit. A provider that answers every request with an error clip longer
  than that therefore keeps the stream alive between switches – `failover.max_switches` is the outer bound for that
  case.
- **Switch limit.** More than `failover.max_switches` switches within `failover.max_switches_window_secs` end the stream.
- When the outage limit or the switch limit ends the stream, the `channel_unavailable` custom video is shown if
  configured.

Each switch emits the event `channel_group.member_switched` (with the reason); start selections emit
`channel_group.member_selected`, exhausted groups `channel_group.exhausted`.

HLS playback switches members only when a new entry is started; HLS runtime failover is not implemented.

## Global settings (`config.yml` → `channel_groups`)

All keys are optional; the block itself can be left out. Every setting is explained with examples in the
[Settings Reference](./channel-groups-settings.md).

| Parameter              | Type | Default   | Description                                                                                                      |
|:-----------------------|:-----|:----------|:-----------------------------------------------------------------------------------------------------------------|
| `match_by`             | Enum | `caption` | Field that decides which channels are equal: `caption`, `name`, `title`, `epg_channel_id`.                       |
| `match_as_ascii`       | Bool | `false`   | Compares match keys after transliteration to ASCII ("Télé" matches "Tele").                                      |
| `match_normalize`      | Map  | empty     | Extra normalization: `strip_regex` (removed from the key), `ignore_tokens` (words ignored), `aliases` (key map). |
| `epg_conflict`         | Enum | `split`   | `split` separates members with different EPG ids into own groups, `ignore` keeps them together.                  |
| `name_strip_quality`   | Bool | `false`   | Removes quality words (`HD`, `FHD`, `4K`, ...) from the group's name.                                            |
| `policy`               | Enum | `auto`    | Member order: `auto` (ranked by QoS, quality, order) or `priority` (configured order, failing members last).     |
| `allow_provider_grace` | Bool | `true`    | A start may use the provider grace slot when no free connection is left. Switches while watching never do.       |
| `start`                | Map  | see below | How a start picks a member.                                                                                      |
| `failover`             | Map  | see below | Switching members while watching.                                                                                |
| `circuit`              | Map  | see below | Pausing failing members and inputs.                                                                              |

### `start`

| Parameter      | Type | Default | Description                                                                          |
|:---------------|:-----|:--------|:-------------------------------------------------------------------------------------|
| `timeout_secs` | Int  | `15`    | Longest time a start may try members before the client gets an error. `0`: no limit. |
| `max_attempts` | Int  | `4`     | Most members tried at start. Must be greater than 0.                                 |

### `failover`

| Parameter                  | Type | Default  | Description                                                                                       |
|:---------------------------|:-----|:---------|:--------------------------------------------------------------------------------------------------|
| `enabled`                  | Bool | `true`   | Switches to another member while a client watches an MPEG-TS stream.                              |
| `no_data_timeout_secs`     | Int  | `8`      | Seconds without media before a member is replaced. Keep it above the keyframe interval. `0`: off. |
| `slow_source_pct`          | Int  | `90`     | Minimum share of real-time speed a member must deliver. `0`: off; at most 100.                    |
| `slow_source_secs`         | Int  | `15`     | How long a member may stay below `slow_source_pct` before it is replaced.                         |
| `max_outage_secs`          | Int  | `20`     | Longest time without playback while looking for a replacement. Must be greater than 0.            |
| `same_source_retries`      | Int  | `1`      | Retries of a member that played stably before other members are tried.                            |
| `stable_after_secs`        | Int  | `30`     | Playback after which a member counts as stable and an incident is over.                           |
| `max_switches`             | Int  | `6`      | Most switches within `max_switches_window_secs`; then the stream ends. `0`: no limit.             |
| `max_switches_window_secs` | Int  | `600`    | Window of `max_switches`.                                                                         |
| `audio_language`           | Enum | `prefer` | Switches `prefer` members with the same audio language, or `require` it.                          |

### `circuit`

| Parameter           | Type | Default | Description                                                                                       |
|:--------------------|:-----|:--------|:--------------------------------------------------------------------------------------------------|
| `source_failures`   | Int  | `2`     | Consecutive failures that pause a member. Must be greater than 0.                                 |
| `cooldown_secs`     | Int  | `60`    | First pause of a failing member; doubles while it keeps failing.                                  |
| `cooldown_max_secs` | Int  | `900`   | Longest pause. Raised to `cooldown_secs` when lower.                                              |
| `input_failures`    | Int  | `3`     | Failures of one input (from at least two members) within the window that put the input last. ≥ 2. |
| `input_window_secs` | Int  | `120`   | Window of `input_failures`.                                                                       |

Changes to grouping settings take effect with the next playlist update; selection and failover settings apply to the
next stream start.

## Target settings (`source.yml` → `targets[].options.channel_groups`)

| Parameter              | Type | Default  | Description                                                                      |
|:-----------------------|:-----|:---------|:---------------------------------------------------------------------------------|
| `enabled`              | Bool | `false`  | Turns channel groups on for this target.                                         |
| `match_by`             | Enum | (global) | Overrides `match_by`, e.g. `epg_channel_id` for a target with clean EPG ids.     |
| `match_as_ascii`       | Bool | (global) | Overrides `match_as_ascii`.                                                      |
| `epg_conflict`         | Enum | (global) | Overrides `epg_conflict`.                                                        |
| `name_strip_quality`   | Bool | (global) | Overrides `name_strip_quality`.                                                  |
| `policy`               | Enum | (global) | Overrides `policy`.                                                              |
| `allow_provider_grace` | Bool | (global) | Overrides `allow_provider_grace`, e.g. `false` for a target with many users.     |
| `failover`             | Bool | (global) | Overrides `failover.enabled`, e.g. `false` for players that stutter on a switch. |

Timing, `start`, `circuit` and `match_normalize` exist only globally: the pause state of a source is shared by all
targets, and the timing describes the providers, not the target.

## Example

```yaml
# config.yml
channel_groups:
  name_strip_quality: true
  match_normalize:
    strip_regex: "^[A-Z]{2}:\\s*"   # drop country prefixes like "DE: "
    ignore_tokens: [hevc, backup]
    aliases:
      "das erste": "ard"
  failover:
    no_data_timeout_secs: 10
    max_outage_secs: 30
```

```yaml
# source.yml
targets:
  - name: family
    options:
      channel_groups:
        enabled: true
      share_live_streams:
        mpeg_ts: true   # optional: one provider connection per group for all viewers
  - name: tv-box
    options:
      channel_groups:
        enabled: true
        failover: false   # this player stutters on a switch; keep the start selection only
```
