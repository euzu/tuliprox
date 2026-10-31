# 📊 Playlist Target Analytics ("Playlist > Stats")

Tuliprox computes content and electronic program guide (EPG) statistics for each target during every playlist update cycle.
These metrics are stored locally in the target storage directory and visualized in the Web UI under **Playlist > Stats**.

---

## 1. Web UI Overview

The **Stats** page is located in the sidebar under the **Playlist** section, directly below **Update**:

- **Update**: Manual playlist refreshes and execution details.
- **Stats**: Content KPIs, input-to-output ratios, EPG source health, and unmapped channel export.
- **Playlist viewer**: Catalog inspection for live channels, VOD, and series.
- **EPG**: Timeline program guide and channel mapping status.
- **Playlist settings**: Target-level playlist configuration and tuning.

### Permissions

- **`playlist.read`**: Required to open the Stats page, inspect target metrics, and download the unmapped channels CSV export.
- **`playlist.write`**: Required to trigger the **Update now** action from the header.
- **`web_ui.landing_page: playlist_stats`**: Can be configured in `config.yml` as the initial dashboard landing page.

---

## 2. Page Layout and Metrics

### 2.1 Header and Target Selection

- **Target selector**: Switches the view between configured targets.
- **Update now button**: Visible only with `playlist.write` permission; triggers `POST /api/v1/playlist/update` for the selected target.
- **Meta line**: Displays configured output types, assigned inputs, relative time and duration of the last update, and the next scheduled run.

### 2.2 Key Performance Indicator (KPI) Cards

1. **Live**: Total live channels and count of distinct channel groups.
2. **Movies**: Total VOD entries and count of distinct categories.
3. **Series**: Total series entries and count of resolved episodes. Episodes are counted only when resolved during processing
   (M3U series entries or Xtream series episodes). When only part of the series is resolved, the count is a lower
   bound shown as "16+" with the tooltip "Only partially resolved"; without any resolved series it shows "—".
4. **EPG Coverage**: Percentage and count of live channels with guide data matched to a retained EPG channel.
5. **Catch-up**: Total live channels offering archive playback, alongside the maximum catch-up archive window in days.

### 2.3 Input vs. Output & Group Distribution

- **Input vs. Output bars**: Compares raw input channel counts against final curated output counts per cluster (`Live`, `Video`, `Series`),
  showing the percentage kept after filtering, mapping, and deduplication.
- **Largest live groups**: Top 5 channel groups by live stream count.

### 2.4 EPG Coverage Panel

- **Coverage ring**: SVG gauge visualizing the percentage and exact counts of mapped versus unmapped live channels.
- **Guide window**: The guide horizon expressed in days and the maximum programme end date across all mapped channels.
- **EPG sources breakdown**: Lists each EPG source contributing to the target:
  - **Input**: Source input name.
  - **Label**: Sanitized file name or host of the EPG URL (credentials and sensitive query parameters are never displayed).
  - **Type**: Format (`XMLTV` or `ICS`).
  - **Guide horizon**: Retained guide span in days.
  - **Last fetch**: Relative time of the last successful fetch, or the last failure timestamp with short error text.
  - **Mapped channels**: Number of target channels drawing guide data from this source.
  - **Status badge**: `OK` or `Error`.
- **Missing EPG by group**: Distribution of unmapped live channels across the top 4 groups plus an aggregated "Other" bucket.
- **Export unmapped channels**: Downloads `analytics_unmapped.csv` for the selected target.

### 2.5 All Targets Table

An overview table summarizes metrics across every configured target:

| Column | Description |
| :--- | :--- |
| **Target** | Target name; clicking a row selects the target in the detail panel. |
| **Output** | Configured output formats (`Xtream`, `M3U`, `HDHomeRun`, `STRM`). |
| **Live** | Live stream count. |
| **Movies** | Movie (VOD) count. |
| **Series** | Series count. |
| **Episodes** | Episode count when known, "N+" when partially resolved, otherwise "—". |
| **EPG** | Visual coverage bar and percentage. |
| **Updated** | Relative timestamp of the last update run. |
| **Status** | Operational status badge. |

---

## 3. Status Classification Rules

Each target displays one of four operational status badges:

| Status | Condition |
| :--- | :--- |
| **`Error`** | The most recent playlist update run for the target failed (`success: false`). |
| **`EPG stale`** | An assigned EPG source failed during the last update run, or the retained guide window ends within 24 hours. |
| **`OK`** | The target updated successfully and its guide data extends beyond 24 hours without source errors. |
| **`No data`** | The target has never been updated yet (`analytics: null`). |

---

## 4. Storage Files

Analytics data is persisted during target finalization:

| File Path | Description |
| :--- | :--- |
| `<storage_dir>/<target_name>/analytics.json` | JSON snapshot of `TargetAnalyticsDto`, written atomically via temporary file and rename. |
| `<storage_dir>/<target_name>/analytics_unmapped.csv` | RFC 4180 CSV export of unmapped live channels with header `group,name,epg_channel_id,input`. |
| `<storage_dir>/input_<input_name>/status.json` | Input status file containing `epg_sources` with sanitized source labels, fetch timestamps, and errors. |

Writing analytics is non-blocking: storage write errors are logged as warnings and do not fail target publication.

- **Published run**: new metrics and a new unmapped CSV are written.
- **Failed run**: previous metrics are kept and marked `success: false`; without a previous snapshot, this run's
  metrics are written as failed. The CSV is not touched.
- **Empty run** (nothing published): previous metrics and CSV are kept, only the run time is updated.
- CSV fields starting with `=`, `+`, `-` or `@` are prefixed with `'` so spreadsheets do not evaluate them.

---

## 5. REST API Endpoints

Target analytics and unmapped exports are exposed through the v1 REST API:

### `GET /api/v1/playlist/analytics`

Returns summary analytics, next scheduled update times, and execution details for all configured targets.

- **Permission**: `playlist.read`
- **Response**: JSON array wrapped in `{"targets": [...]}`. Targets without prior updates return `analytics: null`.

### `GET /api/v1/playlist/analytics/{target_id}/unmapped.csv`

Streams the stored CSV file of live channels without guide data for the given target ID.

- **Permission**: `playlist.read`
- **Response**: `text/csv; charset=utf-8` stream with
  `Content-Disposition: attachment; filename="{sanitized_target_name}-unmapped.csv"`; when sanitizing leaves an empty
  name, the file is named `target-{target_id}-unmapped.csv`.
- **Status Codes**: `200 OK` on success, `404 Not Found` if no unmapped CSV exists.
