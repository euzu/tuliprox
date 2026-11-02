# 🎛️ Playlist Item Overrides & the Playlist Explorer

A mapper script is the right tool for rules that apply to many channels. It is the wrong tool for one broken
entry: a channel whose name the provider mangled, a movie with the wrong rating, a live stream whose EPG id points
nowhere. Until now, fixing such an item meant writing a rule that matched exactly this one item — and then keeping
that rule alive while the provider's catalog drifted.

Manual item overrides are the other tool: a small YAML file per input or per target that holds field changes for
individual items, keyed by a stable item id. Overrides are applied during playlist processing, exactly like the
output of a mapper, but they are authored per item — in the Playlist Explorer, over the REST API, or through the
MCP tools — and they survive provider renames, aliases and playlist updates.

- **Input scope** corrects an item once for every target that uses the input.
- **Target scope** corrects an item in one target only, and its values stay locked against later stages of that
  target's pipeline.
- **No scripts involved**: no filter, no regex, no rule that can accidentally catch a second channel.

The Web UI surface for this feature is the redesigned **Playlist Explorer**, described in
[section 12](#12-the-playlist-explorer).

---

## 1. Where override files live

Override files are plain YAML below the `overrides` directory next to `config.yml` (the directory is resolved
from the configured config path):

```text
<config dir>/overrides/input/<input name>.yml     # applies to every target using the input
<config dir>/overrides/target/<target name>.yml   # applies to one target
```

The file name is the configured input name (the *parent* input, see [section 6](#6-item-identity)) or the
configured target name, percent-encoded: every byte outside `[A-Za-z0-9_-]` becomes `%XX` (upper-case hex).
`Provider A` therefore lives in `input/Provider%20A.yml`. A name whose encoding exceeds 240 bytes is rejected,
and an empty name is rejected as well.

Files are created on the first save and removed again once the last entry of a file is gone, so an empty
override file never exists on disk. Both directories are optional; without them nothing is overridden.

---

## 2. File format

A file is a YAML mapping with `schema_version: 1` and the item sections `live`, `vod` and `series`. Each section
maps an override id (a string key — quote numeric provider ids) to the changed fields of that item.

### 2.1 Input override file

```yaml
schema_version: 1
live:
  "12345":
    _hint: "UK News / BBC One HD"
    name: BBC One
    group: UK | News
    epg_channel_id: bbc.one
  "98765":
    _hint: "Sports / Sky Sports F1 HD"
    chno: 101
    logo: https://cdn.example.com/logos/f1.png
vod:
  "5555":
    _hint: "Movies / Blade Runner 2049"
    title: Blade Runner 2049
    rating: 8.4
    release_date: 2017-10-06
series: {}
```

### 2.2 Target override file

A target file carries one more level: `inputs:` maps the parent input name to the same three sections. An item is
therefore addressed by `target file → input → section → id`.

```yaml
schema_version: 1
inputs:
  provider_a:
    live:
      "12345":
        _hint: "UK News / BBC One HD"
        title: BBC One HD (a)
        chno: 3
    vod:
      "5555":
        _hint: "Movies / Blade Runner 2049"
        rating: 9.0
```

Sections that hold no entries are omitted when tuliprox writes the file; an entry that holds no field left is
dropped. Both files are written in sorted key order, so a diff stays readable.

### 2.3 Set, clear and keep

Every editable field has three states:

- **absent** — the field is not touched; the provider value reaches the output.
- **a value** — the field is replaced with it.
- **`null`** — the field is cleared: at the output the field is empty (no EPG id, empty logo, rating `0` and so
  on). `name`, `title` and `group` cannot be cleared; a `null` there is a validation error.

```yaml
live:
  "12345":
    epg_channel_id: null   # remove the wrong EPG id, keep everything else
    name: BBC One          # replace the provider name
```

### 2.4 `_hint`

`_hint` is informational only and is never applied. It records `"<group> / <title>"` of the raw provider item at
the time the entry was written, so a human — or the Explorer — can tell which item an id belonged to after the
provider renamed it. The API fills the hint from the stored input inventory when it can find the item, and falls
back to the hint the request carried otherwise.

### 2.5 What makes a file unusable

The reader is strict about structure and lenient about single fields:

- A root key other than `schema_version`, `live`, `vod`, `series` (input file) or `schema_version`, `inputs`
  (target file) makes the file **malformed**; the same holds for a section that is not a mapping, an entry that is
  not a mapping, a missing `schema_version`, or a `schema_version` that is not an integer.
- A `schema_version` other than `1` makes the file **unsupported**. Such a file is never patched and never applied.
- An unknown or wrongly typed field inside an entry does not break the file. The value is kept as an unknown key,
  reported as a field error (`error.unknown_field`, `error.invalid_type`), and everything else of the entry is
  applied. A malformed file is logged and skipped by the run; the state is recorded in the override status
  document of the target.

A file that is malformed or carries an unsupported schema version is never rewritten: the repository refuses to
patch it, so a hand-made file cannot silently lose its content. Fix or delete it instead.

---

## 3. Editable fields

The allowed fields depend on the item kind. `live` covers live channels, `vod` covers movies and `series` covers
series entries.

- **All kinds**: `name`, `title`, `group`, `logo`.
- **Live only**: `logo_small`, `epg_channel_id`, `chno`, `parent_code`, `audio_track`, `time_shift`, `rec`.
- **VOD and series only**: `tmdb`, `plot`, `genre`, `cast`, `director`, `release_date`, `rating`.

Validation rules, checked on every save and reported as field errors (`422`):

- `name`, `title`, `group` — set only, non-empty, no leading or trailing whitespace.
- `logo`, `logo_small` — an `http` or `https` URL with a host.
- `epg_channel_id` — non-empty and not blank.
- `chno` — 1 to 999999.
- `parent_code`, `audio_track`, `time_shift`, `rec` — free text.
- `tmdb` — an integer of 1 or greater.
- `release_date` — `YYYY-MM-DD`.
- `rating` — between 0 and 10.
- `plot`, `genre`, `cast`, `director` — free text.

A field that is not allowed for the item kind (for example `tmdb` on a live channel) is rejected with
`error.field_not_allowed`; it is not silently dropped.

How the applied values land in the output:

- `logo` fills the stream icon of live channels, the image of a movie and the cover of a series; `logo_small`
  fills the small logo of a live channel.
- `rating` also fills the 5-based rating (`rating / 2`) of a movie or series.
- `chno` is applied before the target assigns channel numbers, so a manual number is kept and the automatic
  assignment skips it (a duplicate manual number is logged as a warning, not rejected). Clearing `chno` returns
  the item to automatic numbering.
- `parent_code`, `audio_track`, `time_shift` and `rec` are used by the player, catch-up and recording paths
  exactly as provider values are.

---

## 4. Scopes

Two files can hold an entry for the same item, and both are applied: input first, then target, field by field.
The more specific value wins — a target entry overrides an input entry for the same field, but leaves the
input's other fields in force.

- **Input scope** (`overrides/input/<input>.yml`): the correction applies in every target that uses the input,
  including targets added later. Use it when the provider data is simply wrong.
- **Target scope** (`overrides/target/<target>.yml`): the correction applies in one target only. Use it when a
  target needs a different presentation of the same provider item.

The Playlist Explorer's edit panel therefore asks for the scope before a field is changed: *for input (all
targets)* or *this target only*. The target scope is only offered in the target view of the Explorer.

---

## 5. Pipeline order

Overrides are part of playlist processing, not a post-processing patch — the target's playlist is built from the
edited items.

1. The input override files are loaded once per processing run, the target override file once per target.
2. Before the target pipeline runs, input and then target overrides are applied to the raw input items.
3. The target pipeline (filter, rename, mapper) runs on the **edited** values. A filter therefore matches what a
   user sees, not the provider's original text: filtering on `group: Sports` keeps a channel whose group an
   override moved to `Sports`, and filtering out `group: Adult` removes one an override moved there.
4. After every later stage that rewrites item fields — VOD/series detail resolution, EPG assignment, the
   after-EPG mapping stage, guide-id case adoption, invalid-EPG-id clearing, curation and the appearance merge —
   the target's override fields are re-applied for the affected items, and items whose group no longer matches
   their playlist group are moved into the group the override names. Rename and mapper rules cannot undo a target
   override this way.
5. Sorting and channel-number assignment run on the re-applied values, so a manual `chno` wins and stays assigned;
   counter assignment re-applies the other fields but deliberately keeps the manual number.

Input overrides are not re-applied after rename and mapping: they are the provider-side correction that a target
may still shape, while a target override is the final word for its target.

---

## 6. Item identity

An override is keyed by the parent input plus `(kind, id)`, so it stays valid across playlist updates as long as
the provider keeps the item's id.

- **Xtream and other non-M3U inputs** use the provider stream id: `live` uses `stream_id`, `vod` uses the movie
  id, `series` uses the series id.
- **M3U inputs** use the M3U item's id when it is genuinely the item's identity (the numeric id in the URL, or a
  short hash that equals the id found in the URL). Otherwise the id is computed from the URL as
  `urlhash-v1:<blake3 hex>` (see below).
- **Aliases share identity.** An input with `aliases` produces items whose runtime input name is the alias name,
  but the parent input name is what the override key uses — an override written for `provider_a` therefore keeps
  working for an item that arrives through the alias account of the same panel.

### 6.1 `urlhash-v1`

M3U lists frequently repeat ids, omit them, or embed the server, the port and the account in every URL. The
`urlhash-v1` identity is computed from a canonical form of the URL:

1. parse the URL and drop scheme, user info, **host and port**;
2. split the path into segments and drop a whole segment only when it equals one of the input's credentials
   (user name, password, and `username` / `password` query values, including their percent-decoded forms);
3. percent-decode the query, drop the `username` and `password` parameters and any parameter whose value equals a
   credential, keep the rest and sort them by name, then value;
4. re-encode with RFC 3986 unreserved escaping as `<path>?<query>`;
5. hash the result with BLAKE3 and prefix it with `urlhash-v1:`.

The hash is stable across accounts on the same panel (credentials and host play no part), but it changes when the
stream path itself changes. Reads of the stored inventory never return a provider URL, and a field whose value
looks credential-bearing (a logo URL carrying an account, for example) is blanked instead of shown.

> **Accounts in the URL path.** Only credentials tuliprox knows are removed: the `username` / `password` of the
> input and its aliases, and the `username` / `password` query values. When an M3U input carries its account only
> inside the stream path (`http://host/live/<user>/<pass>/abc123.m3u8`) and its items have no provider id, set
> `username` and `password` on the input (and on each alias). Otherwise the account segments become part of the
> hash, and swapping the account changes every `urlhash-v1` key and orphans the overrides of that input.

---

## 7. Ambiguous ids

An override key is unique per parent input, but a provider inventory is not: the same M3U id can appear twice, and
two items may share a computed `urlhash-v1`. When an override key resolves to more than one item of an input, the
override is **not applied to any of them** — applying it to an arbitrary one would be worse than not applying it.
The run logs the ambiguous key and counts it in the target's override diagnostics, and the Explorer marks such an
item as ambiguous instead of showing edited values.

Resolve ambiguity by removing the duplicate from the playlist (filter) or by using a target scope for a target
that only carries one of the two items.

---

## 8. Orphans

Inventory moves on: a provider drops a channel, a target is renamed, an input is removed. The override entries
are file content and do not disappear with it. The Explorer and
`GET /api/v1/playlist/overrides/orphans` classify every entry of an input:

- **active** — the item exists in the stored input inventory.
- **orphaned** — the input loaded successfully and no longer contains the item. The entry can be discarded.
- **unknown** — the input has no reliable stored inventory (it was never updated, the last run failed, or a
  record could not be read). Nothing is claimed about the entry; it may still apply after the next update.

For an input with `aliases` the inventory is the union of the parent and every enabled alias, because each alias
account keeps its own stored playlist. When one of them has no reliable inventory, every entry is `unknown`.
Input names in the override API and the Explorer may be given as the parent or as any alias name.

Orphan classification is deliberately conservative: a missing database or an unreadable record makes the whole
scan non-authoritative, and every entry is then reported as `unknown` instead of `orphaned`.

**Orphaned files** are a second category: override files whose name matches no configured input or target, for
example after a rename. They are listed with their revision and can be removed with a discard request that names
the file and the revision.

---

## 9. Revisions, conflicts and pending updates

- **Revision**: a file's revision is the BLAKE3 hex digest of its bytes, so it changes with every write and is
  reproducible from the file itself. A missing file has the revision of the empty byte string.
- **Optimistic concurrency**: every save and every discard carries the revision it was based on. The write is
  performed under a per-file lock that re-checks the on-disk revision immediately before the atomic replace (temp
  file in the same directory, `fsync`, rename, directory `fsync`). A stale revision answers `409` with the current
  revision and changes nothing; re-read and repeat the change. Two browsers editing the same file cannot silently
  overwrite each other.
- **No comments survive a save.** Files are rewritten from the parsed model, so comments and formatting that were
  written by hand in an override file are lost the next time the Web UI, the REST API or an MCP client saves it.
  The values are preserved, including unknown keys the reader kept for you.
- **Pending**: a saved change is not live until the affected target runs a playlist update. The status document
  `<storage dir>/override_status.json` records which revisions each target's last *successful* run applied; an
  entry is pending for a target while its file revision differs from that record. Input edits are therefore
  pending per target, which the Explorer and `get_override_item` report as `pending_per_target`. A failed update
  leaves every revision pending. A target only tracks the input files of the inputs it consumes, so editing an
  unrelated input never marks it pending. Deleting all override files clears the pending state with the next
  successful run, and so does a file without entries. One-shot command line runs record the status too.

`POST /api/v1/playlist/update` (or `update_playlists` over MCP) triggers the run; nothing else applies an edit.

---

## 10. REST API

All endpoints live below `/api/v1`, require the same bearer token as the other endpoints, and are split by
permission: reads need `playlist.read`, writes need `playlist.write`. Names are resolved like everywhere else in
the playlist API — an input or target name that does not exist (or is not visible to the caller) answers `404`.
Get a token first, as shown in the [REST API Cookbook](../rest-api-cookbook.md#example-1-get-a-jwt-token):

```bash
BASE_URL="http://localhost:8901"
TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'accept: application/json' \
    -H 'content-type: application/json' \
    --data-raw '{"username":"admin","password":"12345678"}' | jq -r '.token')
```

### 10.1 Find the override id of an item

`GET /playlist/items` accepts `fields=override_id`. That value — the provider id, or the `urlhash-v1:…` value for
M3U items, or the exact match of `search_field=override_id` — is the id every override endpoint takes.

```bash
curl -s "$BASE_URL/api/v1/playlist/items?target=my_target&cluster=live&search=BBC&fields=override_id&fields=name&fields=title" \
    -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | jq .
```

```json
{
  "source": { "kind": "target", "name": "my_target" },
  "cluster": "live",
  "total_matched": 1,
  "offset": 0,
  "limit": 50,
  "returned": 1,
  "has_more": false,
  "items": [ { "override_id": "12345", "name": "BBC One HD", "title": "BBC One HD" } ]
}
```

### 10.2 Read an override file

```bash
curl -s "$BASE_URL/api/v1/playlist/overrides?scope=target&name=my_target" \
    -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | jq .
```

```json
{
  "revision": "6f1c…",
  "entries": [
    {
      "input": "provider_a",
      "kind": "live",
      "id": "12345",
      "hint": "UK News / BBC One HD",
      "fields": { "title": "BBC One HD (a)", "chno": 3 }
    }
  ],
  "field_errors": []
}
```

`fields` is the stored delta of the entry: a value means set, `null` means cleared. `parse_error` (a string) or
`schema_error` (a version number) replace `entries` when the file cannot be used at all; such a file is never
patched.

### 10.3 Save patches

A save carries the base revision and one patch per item. Each patch names `kind`, `id` and the changed fields;
in target scope every patch also names its `input`. Field values are tagged operations, not bare values:

- `{"op": "set", "value": …}` — store this value.
- `{"op": "clear"}` — hide the provider value (empty at the output).
- `{"op": "reset"}` — drop this override and use the provider value again.
- an omitted field is left exactly as stored.

```bash
curl -s -X POST "$BASE_URL/api/v1/playlist/overrides" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{
      "scope": "target",
      "name": "my_target",
      "base_revision": "6f1c…",
      "patches": [
        {
          "input": "provider_a",
          "kind": "live",
          "id": "12345",
          "patch": {
            "title": { "op": "set", "value": "BBC One HD (a)" },
            "chno": { "op": "set", "value": 3 },
            "epg_channel_id": { "op": "clear" }
          }
        }
      ]
    }' | jq .
```

A successful save answers `{ "revision": "…" }`; use it as the `base_revision` of the next write.

Error answers:

- `409` — the file changed since `base_revision` (or cannot be patched because it is malformed/unsupported). The
  body is `{ "current_revision": "…", "error": "…" }`.
- `422` — the patch is well-formed but invalid for the item kind: `{ "field_errors": [ { "field", "message_key",
  "entry" } ] }`.
- `400` — empty patch list, duplicate `(input, kind, id)` triple, a target patch without `input`, an input patch
  naming a different input, or a blank id.

### 10.4 Discard entries or a whole file

`POST /playlist/overrides/discard` either removes the listed entries or — with an empty `items` array — deletes
the whole file, which is also how an orphaned file is cleaned up.

```bash
curl -s -X POST "$BASE_URL/api/v1/playlist/overrides/discard" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{
      "scope": "target",
      "name": "my_target",
      "base_revision": "6f1c…",
      "items": [ { "input": "provider_a", "kind": "live", "id": "12345" } ]
    }' | jq .
```

Omit `items` (or send `[]`) together with the revision of an orphaned file to remove that file. A repeated
discard of an absent file is idempotent.

### 10.5 Explain one item

`GET /playlist/overrides/item` shows which layer a value currently comes from: the raw provider record
(`provider`), the rules of the input file (`input_override`), the rules of one target's file (`target_override`),
the record the target processed (`processed`), the pending state per affected target (`pending_per_target`), and
whether the id is `ambiguous`.

```bash
curl -s "$BASE_URL/api/v1/playlist/overrides/item?input=provider_a&kind=live&id=12345&target_id=7" \
    -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | jq .
```

```json
{
  "provider": { "name": "BBC One HD", "title": "BBC One HD", "group": "UK News", "chno": 0 },
  "input_override": { "epg_channel_id": null },
  "target_override": { "title": "BBC One HD (a)", "chno": 3 },
  "processed": { "name": "BBC One HD", "title": "BBC One HD (a)", "group": "UK News", "chno": 3 },
  "pending_per_target": [ [ "my_target", true ] ],
  "ambiguous": false
}
```

### 10.6 Orphans of an input

```bash
curl -s "$BASE_URL/api/v1/playlist/overrides/orphans?input=provider_a" \
    -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | jq .
```

```json
{
  "input": "provider_a",
  "entries": [
    { "scope": "target", "file": "my_target", "input": "provider_a", "kind": "vod", "id": "5555",
      "hint": "Movies / Blade Runner 2049", "state": "orphaned" }
  ],
  "orphaned_files": [ { "scope": "input", "name": "old_provider", "revision": "4c2a…" } ]
}
```

### 10.7 EPG id candidates

`GET /playlist/epg/search` searches stored EPG data for the `epg_channel_id` editor: exactly one of `target_id`
(a processed target EPG database) or `input` (the input's EPG cache), and a required `q`. A prefix match ranks
above a substring match; both the channel id and its display name are searched. `limit` defaults to 20 and is
clamped to 50. No provider is contacted, and a source without stored EPG data answers an empty list.

```bash
curl -s "$BASE_URL/api/v1/playlist/epg/search?input=provider_a&q=bbc&limit=5" \
    -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | jq .
```

```json
[ { "id": "bbc.one", "display_name": "BBC One" }, { "id": "bbc.two", "display_name": "BBC Two" } ]
```

---

## 11. MCP tools

The [MCP server](../operator/mcp.md) exposes the same feature to an MCP client. Reads: `list_playlist_sources`,
`query_playlist_items`, `get_overrides`, `get_override_item`, `list_override_orphans`, `search_epg_ids`. Writes:
`save_overrides`, `discard_overrides`, `update_playlists`. Permission checks and validation are the REST ones —
the tools are thin adapters over the endpoints above.

A typical correction runs in this order:

1. `list_playlist_sources` — find the target or input name.
2. `query_playlist_items` with `cluster: "live"`, `search: "BBC"`, `fields: ["override_id", "name", "group"]` —
   the returned `override_id` is the key for everything below.
3. `get_overrides` with `scope: "target"`, `name: "my_target"` — keep the returned `revision`.
4. `save_overrides` with `scope: "target"`, `name: "my_target"`, that `base_revision`, and one patch per item:
   `{"input": "provider_a", "kind": "live", "id": "12345", "patch": {"title": {"op": "set", "value": "BBC One
   HD (a)"}}}`. A `409` means the file moved on: re-read with `get_overrides` and repeat against the new
   revision instead of retrying blindly.
5. `update_playlists` with `targets: ["my_target"]` — the edit becomes visible with that run.
6. `get_override_item` (or the next `query_playlist_items`) to confirm that `pending_per_target` cleared and the
   processed record carries the new value.

Use `search_epg_ids` before setting `epg_channel_id`, and `list_override_orphans` plus a `discard_overrides`
without `items` to clean up entries and files that no longer match the configuration.

---

## 12. The Playlist Explorer

The Explorer is the playlist browser of the Web UI (**Playlist > Explore** in the sidebar). It shows the stored
playlist of the selected target or input, overlaid with the saved overrides, so what it displays is what the
target publishes — including pending markers.

The Explorer loads its lists with `?explorer=true` on the playlist content endpoints. Only then does the server
attach per-item metadata (override state, EPG state, `override_id`, `added`, `last_modified`, channel number); other callers keep
the compact, streamed response. Editing, bulk edit and cleanup need the `playlist.write` permission; users with
read access only see the details.

### 12.1 Layout

- **Cluster tabs** — `Live`, `VOD`, `Series`, each with its item count. Live uses a compact list, VOD and series
  use a poster grid with a list view toggle.
- **Sidebar** — resizable with its drag handle (the width is remembered per browser); `All groups` plus one entry
  per group with its item count, followed by the virtual groups
  `⚠ Without EPG` (live only) and `✎ Edited` (all clusters). The virtual groups are counts of the items
  currently known, so they collapse to zero once the edits are discarded.
- **Main area** — the item list or grid of the selected group, sorted by the active sort order.
- **Search** — case-insensitive text or regex over selectable fields (`group`, `title`, `name`, `url`,
  `Provider ID`). Searching by provider id is how an item is found when its name is unusable; the field list is
  remembered per browser.
- **Detail panel** — the selected item with its cluster, provider (override) id and input, the scope switch, the
  `edited` marker of an item that carries an override, and the edit form.
- **Row actions** — every table starts with the row action menu (☰) in its first column; the second column holds
  the selection checkbox used by bulk edit.

On narrow screens (phones) the layout changes: the group list becomes a group selector in the toolbar, the
detail and edit panel open full screen with a back button, the live table hides the logo and status columns
(they can be shown again in the columns panel, which keeps a separate layout for phones), and the filter row
scrolls horizontally.

### 12.2 Sorting defaults

- **Live**: channel number, ascending. The number is the one the stored playlist carries (a leading number in
  the channel name is not used); items without one sort last in playlist order.
- **VOD**: `Added`, descending (newest first), using the provider's `added` timestamp.
- **Series**: `Last modified`, descending, using the provider's `last_modified` timestamp, falling back to `added`.

The selector offers channel number, name and group for live, and added/title/year/rating or
last modified/title/year/rating for VOD and series. Switching the cluster resets the sort to that cluster's
default. Where a sort key is missing from an item it sorts last, in playlist order (an input has no channel
numbers, so its live list keeps the provider order); equal values break ties on provider id.

VOD and series also offer a rating filter (`★ ≥ 6`): a click switches it on and off, and the menu next to it picks
the minimum number of stars (1 to 9). Picking a value switches the filter on.

### 12.3 EPG states

Live items carry an EPG badge computed from the target's EPG data at read time:

- **No EPG ID** (`missing_id`) — the item has no `epg_channel_id`.
- **EPG ID not in sources** (`not_in_sources`) — the id is set but no EPG source knows it; typically a mangled id.
- **EPG ID without programme** (`no_programme`) — the channel exists but has no programme covering the current
  time.
- **no badge** (`ok`) — the id is in the guide and has a current programme.

`Without EPG` collects the first three states, so it is exactly the list to work through with the `epg_channel_id`
editor. VOD and series items have no EPG state.

### 12.4 Edited marker and editing

- An item with a saved override is marked `edited` in the list and detail panel; `Edited` lists them per cluster.
  `pending` marks an item whose file changed since the selected target's last successful run.
- **Scope**: the detail panel switches between *For input (all targets)* and *This target only* before a field is
  changed; the target scope is only offered in a target view.
- **Name and title**: the list and Xtream clients show the *title*. When name and title are equal, editing the
  name also sets the title (checkbox *Also set title*, on by default in that case).
- **Provenance**: every field shows the provider value, the input and target override and the effective value with
  its source. In target scope, *Reset* falls back to the input override when one exists.
- **Save**: only changed fields are sent. A conflict (`409`) reloads the current file and tells you to repeat the
  change; validation errors (`422`) are shown at the field. A file that cannot be parsed (hand-edited YAML with a
  syntax error, or an unsupported `schema_version`) is reported in the panel and blocks saving in that scope until
  it is repaired. After a save the item is marked `edited` and `pending`
  at once, and *Update now* starts a playlist update.
- **Bulk edit**: select items with the checkboxes and open *Bulk Edit*. Each field starts at *Unchanged*; choose
  *Set*, *Clear* or *Reset* for the fields to change. `chno`, `name`, `title` and `tmdb` cannot be bulk-edited. In
  input scope a selection that spans several inputs is saved per input after a confirmation that names the inputs
  and counts; the result is reported per input (saved, conflict or validation error).
- **EPG id picker**: the `epg_channel_id` editor suggests candidates from `GET /playlist/epg/search` (prefix
  matches first) instead of asking for a raw id. Typing searches; the value is taken when a suggestion is picked,
  or when the typed id is confirmed with Enter or the field is left. It searches only stored EPG data (the target EPG database or the
  input's cached EPG files) and never contacts a provider.
- **Image preview**: logo, small logo and poster URLs are previewed before they are saved. Provider images load
  through the Web UI resource proxy; a URL you type is loaded by your browser only and is never fetched by the
  server. A failed load shows *could not load in browser*.
- **Orphan view** (*Override Cleanup* in the Explorer toolbar): entries that no longer match the provider inventory are listed with their state
  (`active` / `orphaned` / `unknown`) and can be discarded individually or together; orphaned files are listed
  separately and removed as whole files.

A save is only a file write. It never touches a running stream, and it does not change the served playlists until
the next playlist update of the affected targets — trigger it from **Playlist > Update** (or `update_playlists`)
and watch the pending marker disappear.

---

## 13. Related documentation

- [mapping.yml (Mapper DSL & Logic)](./mapping-dsl.md) — rules for many items, where overrides handle one.
- [source.yml (Inputs, Panel API & Targets)](./source.md) — input aliases and target names used by override
  scopes.
- [Playlist Target Analytics](../operator/playlist-analytics.md) — why an item is missing from a target.
- [MCP Server](../operator/mcp.md) — the tool catalog in full.
- [REST API Cookbook](../rest-api-cookbook.md) — authentication and more endpoint examples.
