# Tuliprox local patch: EPG programme fallback merge

Date: 2026-04-25

## Problem

Tuliprox can load multiple XMLTV sources for one input. The upstream merge logic
selected one complete channel by `epg.sources[].priority`.

When a higher-priority XMLTV source contains a channel ID but has an incomplete
programme range, lower-priority sources for the same channel ID are ignored. This
caused selected Consumer XMLTV IDs to stop early even though another configured
source still had current/future programmes.

Observed affected IDs:

- `zdf.de`
- `3sat.de`
- `arte.de`
- `rtl.de`
- `vox.de`
- `zdfinfo.de`

## Local change

Changed EPG merging so source priority still decides channel metadata
(`title`, `icon`), but programmes from all sources are merged as fallback data
and deduplicated by `(start, stop)`.

Changed files:

- `/root/tuliprox/backend/src/processing/parser/xmltv.rs`
  - `flatten_tvguide()` now keeps higher-priority metadata and fills missing
    programmes from lower-priority sources.
- `/root/tuliprox/backend/src/api/endpoints/v1_api_playlist.rs`
  - Web UI EPG merge now follows the same programme fallback behavior.

## After an upgrade

Re-check whether upstream still has the old behavior:

```bash
cd /root/tuliprox
rg -n "fn flatten_tvguide|merge_epg_channels|guide.priority < acc.priority|priority < acc.priority" backend/src
```

If upstream still replaces the complete channel when a higher-priority source
wins, re-apply this patch.

Expected behavior after patch:

- Higher-priority source wins for channel metadata.
- All sources can contribute programme entries for the same `channel.id`.
- Duplicate programme entries are ignored when `start` and `stop` are identical.

## Verification commands

Run the targeted tests:

```bash
cd /root/tuliprox
cargo test -p tuliprox flatten_tvguide_prefers_higher_priority_metadata_and_merges_all_programmes
cargo test -p tuliprox merge_epg_channels_prefers_higher_priority_metadata_and_merges_all_programmes
```

Then rebuild and replace the installed binary under `/opt/tuliprox` before
refreshing the `German-Only` target.
