# Failover Groups and Stream Failover

Many setups carry the same live channel from several inputs: two providers, two accounts of one provider, or an
HD and an FHD variant. **Failover groups** collapse these equal channels into **one channel** in the target. When a
client plays it, Tuliprox picks the best reachable source ("member") and, if that source fails while the client is
watching, switches to another member **underneath the open response** – the player keeps its connection and does
not have to reconnect.

Failover groups are **turned on per target** and are **off by default**. How channels are matched and how sources are
selected and switched is set **once in `config.yml`** under `failover_groups`; a target can override a few of these
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
          failover_groups:
            enabled: true
```

With this, live channels of `provider_a` and `provider_b` whose caption matches (quality words such as `HD`, `FHD`,
`4K` are ignored) appear once in the `tv` target. All other settings use the defaults of `config.yml`.

In the Web UI:

- **Settings → Failover groups** edits the global block of `config.yml`.
- **Source editor → Targets → *target* → Failover groups** turns the feature on for a target and holds its overrides.
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

The mapping DSL can steer grouping per channel with the header field `failover_group`: a value forces the channel
into the group with that key, an empty string (`""`) keeps the channel out of any group.

### Manual groups and corrections

Automatic grouping is only the base. You can correct an automatic group (order, members, name, logo, policy), dissolve
it, or build your own groups, either in the [Failover groups editor](#editor-web-ui) or by editing YAML files. The
files only hold your deviations from the automatic result; see
[Corrections and own groups (failover files)](#corrections-and-own-groups-failover-files).

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

Each switch emits the event `failover_group.member_switched` (with the reason); start selections emit
`failover_group.member_selected`, exhausted groups `failover_group.exhausted`.

HLS playback switches members only when a new entry is started; HLS runtime failover is not implemented.

## Corrections and own groups (failover files)

Automatic grouping stays the base and keeps working: channels that appear later are grouped automatically, and they
also join an automatic group you corrected. **Failover files** are an overlay on top of it. They store only your
deviations:

- corrections of automatic groups (order, members, name, logo, EPG id, playlist group, policy, dissolving),
- your own **manual groups**,
- channels that must never be grouped automatically (**excluded**).

The failover files are the only place where these decisions live. The `.db` files in `storage_dir`
(including `failover_groups.db`) are derived data: you can delete them at any time, the next playlist update builds
them again from the playlist and the failover files.

Targets without a section in the failover files behave exactly as before.

### Where the files live

Like `mapping_path` and `template_path`, the location is either **one file** or **a directory** with any number of
files.

| Setting                         | Meaning                                                                                                                                                               |
| :------------------------------ | :-------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `failover_path` in `config.yml` | Path to a file or a directory. A relative path is resolved against the config directory. Environment variables are resolved in the path.                              |
| `--failover <PATH>` on the CLI  | Same as `failover_path`, takes precedence over `config.yml`.                                                                                                          |
| nothing configured              | `failover.yml` in the config directory if it exists, else the directory `failover.d` if it exists, else `failover.yml`, which the Web UI creates with the first save. |

```yaml
# config.yml
failover_path: failover.d      # a file or a directory, relative to the config directory
```

If nothing is configured and both `failover.yml` and `failover.d` exist, `failover.yml` is used and a warning names
the ignored directory.

A configured path counts as a directory when it exists as a directory, ends with `/` or `\`, or ends with `.d`.

In directory mode Tuliprox reads all `.yml` and `.yaml` files below the directory (subdirectories included), sorted by
file name. Hidden files and editor temporary files (names starting with `.`, ending with `~`, or ending with `.swp`)
are ignored, so a half-saved editor file is never read. The file names are up to you; `<target>.yml` is the
convention the Web UI uses for new files.

Typical layouts, all equivalent for Tuliprox:

```text
config/failover.yml                    # all targets in one file
config/failover.d/main-tv.yml          # one file per target
config/failover.d/kids.yml
config/failover.d/failover_02.yml      # any name, any number of targets per file
```

Every target is defined in **exactly one** file. The same target twice, in two files or twice in one file, is a load
error that names both files. An empty file, a file that only contains comments, or `targets: []` is valid and
contributes nothing.

The content of failover files is **not** environment-resolved. Placeholders such as `${VAR}` stay as they are.

### File format

The same format is used for a single file and for every file of a directory:

```yaml
version: 1
targets:
  - target: Main TV
    groups:                         # manual groups
      - id: sport-xtra-k3f9
        name: Sport Xtra
        logo: https://example.invalid/sport-xtra.png
        epg_channel_id: sportxtra.example
        playlist_group: Sport
        policy: priority
        members:
          - { input: prov-a, stream_id: "123" }
          - { input: prov-b, stream_id: "9876" }
    auto_overrides:                 # corrections of automatic groups, keyed by auto key
      - auto_key: "zdf"
        policy: auto
        order:  [ { input: prov-b, stream_id: "9876" } ]
        add:    [ { input: prov-c, stream_id: "77" } ]
        remove: [ { input: prov-d, stream_id: "5" } ]
        name: ZDF
      - auto_key: "rtl|rtl.de"      # groups split by EPG id keep the "key|epg" form
        disabled: true              # dissolve this automatic group
    excluded:                       # never grouped automatically (manual groups may still use them)
      - { input: prov-c, stream_id: "55" }
  - target: Kids
    groups: []
```

`version` must be `1` and `target` must be the name of a target from `source.yml`. Channels are always referenced by
**input name** and **provider stream id** (`input`, `stream_id`).

**Manual groups** (`groups`)

| Parameter        | Type   | Required | Description                                                                                                               |
| :--------------- | :----- | :------: | :------------------------------------------------------------------------------------------------------------------------ |
| `id`             | String | Yes      | Stable id: `a-z`, `0-9`, `-`, at most 64 characters, unique within the target. Does not change when you rename the group. |
| `name`           | String | Yes      | Name of the group in the playlist, at most 200 characters.                                                                |
| `logo`           | String | No       | Logo URL, must start with `http://` or `https://`.                                                                        |
| `epg_channel_id` | String | No       | EPG id of the group.                                                                                                      |
| `playlist_group` | String | No       | Playlist group (category) the group appears in.                                                                           |
| `policy`         | Enum   | No       | `auto` or `priority`; without it the setting of the target or `config.yml` applies.                                       |
| `members`        | List   | No       | Channels of the group, in the order that `policy: priority` uses.                                                         |

**Corrections of automatic groups** (`auto_overrides`)

| Parameter                                          | Type   | Description                                                                                                                                                                |
| :------------------------------------------------- | :----- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `auto_key`                                         | String | Identifies the automatic group: its match key. A group that was split by EPG id has the form `key\|epg`, for example `rtl\|rtl.de`. Unique within the target.              |
| `order`                                            | List   | Members that go first, in this order. Other members follow in their natural order.                                                                                         |
| `add`                                              | List   | Channels that join the group although their own key is different.                                                                                                          |
| `remove`                                           | List   | Channels that leave the group.                                                                                                                                             |
| `policy`                                           | Enum   | `auto` or `priority` for this group.                                                                                                                                       |
| `name`, `logo`, `epg_channel_id`, `playlist_group` | String | Replace the value the group takes from its first member. Empty values are ignored.                                                                                         |
| `disabled`                                         | Bool   | `true` dissolves the group. Its channels stay ungrouped and keep their key, so removing `disabled` brings the group back, including channels that arrived in the meantime. |

You rarely have to find an `auto_key` yourself: the Web UI writes it, and `GET /api/v1/failover/groups` returns it
as `key` of every group.

**Excluded channels** (`excluded`) are skipped when automatic groups are built. They can still be members of manual
groups or be added to an automatic group explicitly.

### Precedence

A channel is a member of **at most one** group. If several rules claim it, the first of these wins:

1. a manual group (the first manual group in the file wins when two list the same channel),
2. `add` of an automatic correction,
3. `excluded`,
4. its natural automatic group.

Names of manual groups and automatic keys live in separate namespaces: a manual group `zdf` and an automatic group
with key `zdf` do not collide.

A group still needs at least two usable members. A manual group with fewer than two resolvable channels is inactive
and releases its channels again.

### Problems are reported, not hidden

Nothing is dropped silently. Every playlist update logs a summary per target, and the Web UI shows the details in the
[problems view](#editor-web-ui):

| Problem                      | Meaning                                                                                                                                                                    |
| :--------------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Outdated correction (stale)  | An `auto_overrides` entry whose `auto_key` matches no automatic group any more, for example because the provider or the match settings changed. It has no effect.          |
| Inactive manual group        | A manual group with fewer than two resolvable members.                                                                                                                     |
| Conflicting channel          | A channel claimed by more than one group; only the first claim counts.                                                                                                     |
| Unresolved channel           | A reference in `order`, `add`, `remove` or `members` that matches no channel of the playlist (for example the provider dropped it). The group keeps working with the rest. |
| Dissolved group              | An automatic group switched off with `disabled`.                                                                                                                           |
| Validation finding           | Invalid `id`, empty or too long `name`, invalid `logo`, duplicate `id` or `auto_key`, a channel in two manual groups or in a manual group and an `add` list.               |
| Section of an unknown target | The `target` name matches no target, for example after renaming a target.                                                                                                  |
| Section of a disabled target | The target exists, but failover groups are disabled for it. The section has no effect until you enable failover groups for the target.                                     |

Sections are never deleted automatically, so a renamed target does not lose its corrections. The Web UI offers
**Assign to target…** for such a section, which only rewrites its `target:` field.

If the failover files cannot be loaded when the server starts (YAML syntax error, unsupported `version`, a target
defined twice, unreadable file), the overrides are switched off, playback and automatic grouping keep working, the
playlist update reports the error and the editor shows it in a banner. The Web UI refuses to write to the files until
they load again; it never overwrites a broken file.

### Hot reload

With `config_hot_reload: true` in `config.yml`, Tuliprox watches the failover file or directory.

- A change is picked up without a restart. Creating and deleting files in `failover.d` triggers a reload as well.
- An **invalid** change (syntax error, unsupported version, target defined twice) is rejected: the previous state stays
  active, and an error appears as toast in the Web UI and as `config.reload_failed` event. Fixing the file clears it.
- Changes that only affect **order** or **policy** of existing groups take effect immediately, also when another
  channel becomes the first member: playback follows the new order at once. The playlist entry (name, logo, EPG id,
  category and the other fields taken from the first member) refreshes with the next playlist update. Until then the
  editor shows a hint next to **Apply**, unless the group has `name` (automatic groups), `logo`, `epg_channel_id` and
  `playlist_group` set explicitly in the file. Removing a correction that only held the policy is such a change as
  well; removing an `order` correction needs a playlist update, because the natural order is not stored.
- **Structural** changes (members, metadata, new or dissolved groups, exclusions) need a playlist update, just like
  changes to `mapping.yml`. Run it with **Apply** in the editor, with `POST /api/v1/playlist/update` or with the next
  scheduled update. Until then the editor shows "saved, apply pending".
- Saves from the Web UI are not reloaded a second time.

Limits:

- The watched paths are fixed when the server starts. A `failover.d` directory that is created later, or a changed
  `failover_path`, is only watched after a restart. (A new `failover.yml` next to `config.yml` is noticed.)
- With `config_hot_reload: false` the files are read at start only. Saving in the Web UI still works and detects
  edits that were made on disk.

The directories `mapping.d`, `template.d` and `failover.d` are handled alike: `.yml` and `.yaml` files trigger a
reload, hidden and editor temporary files are skipped.

### Writing from the Web UI

When you save in the editor, Tuliprox rewrites the file that owns the target's section:

- Other sections in the same file are kept (semantically), but **comments and formatting of that file are lost**,
  like for every other config file the Web UI writes. If a file is shared by several targets, the editor tells you
  once per session before the first save.
- A target without a section gets a new file `failover.d/<target>.yml` in directory mode. In single-file mode the
  section is appended to the file, which is created if missing.
- If a section becomes empty (no groups, corrections or exclusions), it is removed. In directory mode a file without
  sections is deleted; in file mode the file keeps `targets: []`.
- The write is atomic and checks that the section still has the content the editor started from. Edits made on disk
  in the meantime are detected as a conflict instead of being overwritten.

Keep hand edits in files that the Web UI does not write often, or use one file per target, if you want to keep your
comments.

## Editor (Web UI)

The page **Failover groups** (sidebar → *Playlist* → *Failover groups*) edits the failover files. It shows
targets that have failover groups enabled and needs the `playlist.read` permission; saving and applying need
`playlist.write`. Without it the page is read-only.

Before the editor can list channels, the target needs one playlist update with failover groups enabled
("Run a playlist update first" otherwise). That update stores the channel catalog the editor works on. No provider is
contacted while you edit.

### Layout

- **Header.** Target selection, group search, filters (*All*, *Automatic*, *Edited*, *Manual*, *Dissolved*,
  *Problems* with a counter), the file that owns the section and its state (*unsaved changes*, *saved, apply
  pending*, *applied*, *file error*), and the actions **New group**, **Undo**, **Refresh preview**, **Discard
  changes**, **Save**, **Save & apply** and **Apply**.
- **Group list.** All effective groups of the target with their origin (*Automatic*, *Edited*, *Manual*,
  *Dissolved*) and the number of usable members.
- **Group detail.** Name, logo, EPG id, playlist group and *Source order* (`auto` or `priority`) with a hint for the
  current value; **Reset to automatic** for edited automatic groups; **Dissolve group** for automatic groups and
  **Delete group** for manual groups.
- **Member table.** Members in their effective order. Reorder by drag and drop or with the row actions *Move up*,
  *Move down*, *Move to first place*, *Move to…* another group, *Remove from group* and *Exclude*. The columns show
  input, title, resolution, codec, bitrate, QoS (24 h / 7 d), the status of the member (including pauses of
  the [circuit breaker](#how-a-source-is-selected)) and the used and maximum connections of its input. They are loaded
  from in-memory data for the selected group only.
- **Candidate panel.** Search all channels of the target and filter them by *Ungrouped*, *Grouped* or *Excluded*.
  Add channels to the selected group, move them out of another group, or create a **new group from the
  selection**.

### What an action writes

Every action changes a draft of the target's section. Nothing is written before **Save**.

| Action                          | Automatic group                             | Manual group                          |
| :------------------------------ | :------------------------------------------ | :------------------------------------ |
| Reorder, move to first place    | `order`                                     | order of `members`                    |
| Add a channel                   | `add`                                       | appended to `members`                 |
| Remove a channel                | `remove`                                    | removed from `members`                |
| Move a channel to another group | remove from the first, add to the second    | same                                  |
| Exclude a channel               | `excluded`, plus `remove` if it is a member | removed from `members` and `excluded` |
| Name, logo, EPG id, policy      | fields of the correction                    | fields of the group                   |
| Dissolve                        | `disabled: true`                            | delete group                          |
| Reset to automatic              | delete the correction                       | -                                     |

New manual groups get an `id` made from their first name plus a short random suffix; it stays the same when you
rename the group.

### Preview

After each change the editor asks the server which groups the draft would produce and shows them in the list and
the table, with **Updating preview…** while it waits. The preview uses the same grouping code as the playlist update
and writes nothing. It shows the effect on groups and members; it does not tell you whether the playlist already
contains it.

### Save and apply

- **Save** writes the YAML file. If the change only affects order or policy of existing groups, it is applied at once
  ("Saved and applied"), also when another channel becomes the first member. Running streams keep their member; new
  selections use the new order. The playlist entry of a group with a new first member refreshes with the next playlist
  update; the editor shows a hint until then.
- Otherwise the state is *saved, apply pending*. **Apply** (or **Save & apply**) queues a playlist update for this
  target, which respects the input cache. If another update is already queued, the editor tells you and you can
  retry.
- The header shows *applied* once the playlist was built from the saved section.
- After a hot apply that changed the first member of a group, the header shows "The new order is active. Name and logo
  of the playlist entry update with the next playlist update." and the affected groups carry a marker in the group
  list. The next playlist update clears both. When a playlist update for the open target finishes (success or
  partial), the editor reloads its preview by itself; updates of other targets and failed runs do not trigger it.

### Problems view

The **Problems** filter lists, for the selected target:

- outdated corrections: **Convert to manual group** (builds a manual group from the references of the correction) or
  **Remove correction**,
- inactive manual groups: **Open group** or **Delete group**,
- conflicting channels (**Remove from** the group that should not claim the channel) and unresolved channels
  (**Remove reference**),
- validation findings,
- sections of unknown targets and of targets with failover groups disabled (listed separately, with the note "target has
  failover groups disabled"), with **Assign to target…** and **Delete section**.

### Changes made elsewhere

If the section changes on disk (hand edit, hot reload, a second browser tab) while you have unsaved changes, a
dialog offers **Reload (discard my changes)** or **Keep editing**. Saving is rejected until you reload, so nothing is
overwritten silently. Without unsaved changes the page reloads by itself. Leaving the page with unsaved changes asks
for confirmation.

### Deep links

- Source editor → *Targets* → *target* → *Failover groups* has a **Failover groups** button that opens the page for
  this target. It appears when failover groups are enabled and you may read playlists.
- For channels of a target with failover groups, the context menu of the playlist explorer offers **Open failover group** (searches the group of the
  channel) and **Add to failover group…** (preselects the channel as candidate).
- The path of the failover files is a field of the main configuration page (**Failover Path**).

### Interfaces for scripts

The editor uses the REST API below `/api/v1/failover/`; see the
[REST API Cookbook](../rest-api-cookbook.md#failover-groups-editor). When a failover file changes on disk, or when
anyone saves or resets a section, the server sends the event `failover.overrides_changed` with the affected
targets (`{"targets": ["Main TV"]}`) over the Web UI WebSocket to clients with `playlist.read`. The editor uses it to
reload by itself. Like the other `failover_group.*` events it is not part of the notification pipeline.

## Global settings (`config.yml` → `failover_groups`)

All keys are optional; the block itself can be left out. Every setting is explained with examples in the
[Settings Reference](./failover-groups-settings.md).

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

## Target settings (`source.yml` → `targets[].options.failover_groups`)

| Parameter              | Type | Default  | Description                                                                      |
|:-----------------------|:-----|:---------|:---------------------------------------------------------------------------------|
| `enabled`              | Bool | `false`  | Turns failover groups on for this target.                                        |
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
failover_groups:
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
      failover_groups:
        enabled: true
      share_live_streams:
        mpeg_ts: true   # optional: one provider connection per group for all viewers
  - name: tv-box
    options:
      failover_groups:
        enabled: true
        failover: false   # this player stutters on a switch; keep the start selection only
```
