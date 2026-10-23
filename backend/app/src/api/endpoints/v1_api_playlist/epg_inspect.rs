//! Read-only inspection of the per-target EPG databases to find errors in EPG processing:
//! `GET /playlist/epg/channels` (issue-filtered channel list) and `GET /playlist/epg/channel` (programmes of one
//! channel).
//!
//! Only the processed per-target EPG database is inspected; input-level raw EPG files are out of scope.

use super::{
    inspect_common::{
        parse_enum, parse_enum_list, query_enum, CaseInsensitiveNeedle, InspectError, PageWindow, DEFAULT_PAGE_LIMIT,
        MAX_PAGE_LIMIT,
    },
    items::StoredPlaylistRecord,
    recording::find_target_by_name,
};
use crate::{
    api::{
        endpoints::{
            extract_form_query::FormQuery,
            xmltv_api::{get_epg_path_and_type_for_target, get_epg_path_for_target_by_type},
        },
        model::AppState,
    },
    model::{Config, ConfigTarget},
    repository::{
        epg_query_channels_by_storage_key, iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist,
        BPlusTreeQuery,
    },
    utils::{canonicalize_untrusted_epg_id, with_folded_epg_id, EpgIdOutputCase, FileLockManager},
};
use axum::{extract::State, response::IntoResponse};
use chrono::{DateTime, SecondsFormat, Utc};
use log::error;
use serde::{Deserialize, Serialize};
use shared::{
    error::TuliproxError,
    model::{ConfigTargetOptions, EpgChannel, EpgProgramme, TargetType, XtreamCluster},
};
use std::{collections::HashSet, path::Path, sync::Arc};
use tokio::task;
use tokio_stream::{Stream, StreamExt};

const DEFAULT_PROGRAMME_LIMIT: u32 = 100;
const MAX_PROGRAMME_LIMIT: u32 = 1000;
const MAX_PROGRAMME_DESC_CHARS: usize = 500;
const DEFAULT_WINDOW_SECS: i64 = 24 * 3600;

query_enum! {
    /// Problems the channel list can be filtered for.
    pub(in crate::api::endpoints) enum EpgIssue {
        NoProgrammes => "no_programmes",
        Expired => "expired",
        NotCurrent => "not_current",
        NoTitle => "no_title",
        Unreferenced => "unreferenced",
    }
}

query_enum! {
    /// Target outputs that have an EPG database.
    pub(in crate::api::endpoints) enum EpgOutput {
        Xtream => "xtream",
        M3u => "m3u",
    }
}

impl EpgOutput {
    const fn target_type(self) -> TargetType {
        match self {
            Self::Xtream => TargetType::Xtream,
            Self::M3u => TargetType::M3u,
        }
    }
}

const fn output_name(output: TargetType) -> &'static str {
    match output {
        TargetType::Xtream => "xtream",
        TargetType::M3u => "m3u",
        TargetType::Strm => "strm",
        TargetType::HdHomeRun => "hdhomerun",
    }
}

/// Casing the target writes EPG channel ids in, i.e. the casing of the keys in its EPG database.
pub(super) fn target_epg_case(target: &ConfigTarget) -> EpgIdOutputCase {
    EpgIdOutputCase::from_lowercase(target.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_epg_ids))
}

fn format_rfc3339(timestamp: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(timestamp, 0).map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Truncates at a character boundary and marks the cut with an ellipsis.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// Scans every channel of an EPG database under its read lock, folding them into an accumulator.
///
/// The b+tree offers no keys-only iteration, so each value is decoded; the cost is O(EPG size) and the memory is
/// bounded by the accumulator. The lock is released before the result is returned.
async fn scan_epg_db<A, F>(
    file_locks: &FileLockManager,
    epg_path: &Path,
    init: A,
    mut visit: F,
) -> Result<A, TuliproxError>
where
    A: Send + 'static,
    F: FnMut(&mut A, Arc<str>, EpgChannel) + Send + 'static,
{
    let file_lock = file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();
    task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path).map_err(|err| {
            TuliproxError::RepositoryEpg(format!("Cannot open EPG database {}: {err}", epg_path.display()))
        })?;
        let mut accumulator = init;
        for entry in query.iter() {
            let (key, channel) = entry.map_err(|err| {
                TuliproxError::RepositoryEpg(format!("Cannot read EPG database {}: {err}", epg_path.display()))
            })?;
            visit(&mut accumulator, key, channel);
        }
        Ok(accumulator)
    })
    .await
    .map_err(|err| TuliproxError::RepositoryEpg(format!("EPG scan task failed: {err}")))?
}

/// The channel ids (storage keys) of an EPG database.
pub(super) async fn load_epg_keys(
    file_locks: &FileLockManager,
    epg_path: &Path,
) -> Result<HashSet<Arc<str>>, TuliproxError> {
    scan_epg_db(file_locks, epg_path, HashSet::new(), |keys: &mut HashSet<Arc<str>>, key, _channel| {
        keys.insert(key);
    })
    .await
}

/// What is known about one EPG channel, computed once and used for filtering and rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelSummary {
    id: Arc<str>,
    title: Option<Arc<str>>,
    has_icon: bool,
    programme_count: usize,
    first_start: Option<i64>,
    last_stop: Option<i64>,
    covers_now: bool,
    has_title: bool,
    /// `None` when the target's playlist is not stored, so the reference is unknown.
    referenced: Option<bool>,
}

/// Whether `now` lies inside `[start, stop)`.
const fn programme_covers(programme: &EpgProgramme, now: i64) -> bool { programme.start <= now && now < programme.stop }

fn summarize(id: Arc<str>, channel: &EpgChannel, now: i64, referenced: Option<bool>) -> ChannelSummary {
    ChannelSummary {
        id,
        title: channel.title.clone(),
        has_icon: channel.icon.as_deref().is_some_and(|icon| !icon.trim().is_empty()),
        programme_count: channel.programmes.len(),
        first_start: channel.programmes.iter().map(|programme| programme.start).min(),
        last_stop: channel.programmes.iter().map(|programme| programme.stop).max(),
        covers_now: channel.programmes.iter().any(|programme| programme_covers(programme, now)),
        has_title: channel.title.as_deref().is_some_and(|title| !title.trim().is_empty()),
        referenced,
    }
}

impl ChannelSummary {
    fn has_issue(&self, issue: EpgIssue, now: i64) -> bool {
        match issue {
            EpgIssue::NoProgrammes => self.programme_count == 0,
            EpgIssue::Expired => self.last_stop.is_some_and(|stop| stop < now),
            EpgIssue::NotCurrent => !self.covers_now,
            EpgIssue::NoTitle => !self.has_title,
            EpgIssue::Unreferenced => self.referenced == Some(false),
        }
    }

    fn has_any_issue(&self, issues: &[EpgIssue], now: i64) -> bool {
        issues.is_empty() || issues.iter().any(|issue| self.has_issue(*issue, now))
    }

    fn matches_search(&self, needle: Option<&CaseInsensitiveNeedle>) -> bool {
        needle.is_none_or(|needle| {
            needle.is_in(&self.id) || self.title.as_deref().is_some_and(|title| needle.is_in(title))
        })
    }
}

#[derive(Serialize)]
struct EpgChannelMeta {
    id: Arc<str>,
    title: Option<Arc<str>>,
    has_icon: bool,
    programme_count: usize,
    first_start: Option<String>,
    last_stop: Option<String>,
    covers_now: bool,
}

impl From<&ChannelSummary> for EpgChannelMeta {
    fn from(summary: &ChannelSummary) -> Self {
        Self {
            id: Arc::clone(&summary.id),
            title: summary.title.clone(),
            has_icon: summary.has_icon,
            programme_count: summary.programme_count,
            first_start: summary.first_start.and_then(format_rfc3339),
            last_stop: summary.last_stop.and_then(format_rfc3339),
            covers_now: summary.covers_now,
        }
    }
}

#[derive(Serialize)]
struct EpgChannelRow {
    #[serde(flatten)]
    meta: EpgChannelMeta,
    referenced_by_playlist: Option<bool>,
}

impl From<&ChannelSummary> for EpgChannelRow {
    fn from(summary: &ChannelSummary) -> Self {
        Self { meta: EpgChannelMeta::from(summary), referenced_by_playlist: summary.referenced }
    }
}

/// Where the EPG of a target is stored.
#[derive(Debug, PartialEq, Eq)]
enum EpgDb {
    /// None of the target's outputs produces an EPG.
    NoEpgOutput,
    /// The output exists, but its EPG database has not been written yet.
    Missing(TargetType),
    Available(TargetType, std::path::PathBuf),
}

impl EpgDb {
    fn output(&self) -> Option<&'static str> {
        match self {
            Self::NoEpgOutput => None,
            Self::Missing(output) | Self::Available(output, _) => Some(output_name(*output)),
        }
    }

    /// Explains an absent database so an empty result is not mistaken for "no problems".
    fn missing_note(&self, target: &str) -> Option<String> {
        match self {
            Self::NoEpgOutput => Some(format!(
                "Target '{target}' has no output with EPG support (xtream or m3u); there is no EPG database to inspect."
            )),
            Self::Missing(output) => Some(format!(
                "No EPG database exists yet for the {} output of target '{target}' (the target was never updated or \
                 no EPG source is configured); an empty result does not mean the EPG is fine.",
                output_name(*output)
            )),
            Self::Available(..) => None,
        }
    }
}

fn resolve_epg_db(config: &Config, target: &ConfigTarget, requested: Option<EpgOutput>) -> Result<EpgDb, InspectError> {
    if let Some(requested) = requested {
        let output = requested.target_type();
        if !target.has_output(output) {
            return Err(InspectError::bad_request(format!(
                "Target '{}' has no {} output",
                target.name,
                output_name(output)
            )));
        }
        return Ok(match get_epg_path_for_target_by_type(config, target, output) {
            Some(path) => EpgDb::Available(output, path),
            None => EpgDb::Missing(output),
        });
    }
    if let Some((output, path)) = get_epg_path_and_type_for_target(config, target) {
        return Ok(EpgDb::Available(output, path));
    }
    Ok(target
        .output
        .iter()
        .map(TargetType::from)
        .find(|output| output.supports_epg())
        .map_or(EpgDb::NoEpgOutput, EpgDb::Missing))
}

/// Adds the id in the casing the EPG database uses for its keys.
fn insert_canonical_id(ids: &mut HashSet<Arc<str>>, id: &str, case: EpgIdOutputCase) {
    let mut insert = |canonical: &str| {
        if !ids.contains(canonical) {
            ids.insert(Arc::from(canonical));
        }
    };
    match case {
        EpgIdOutputCase::Preserve => insert(id),
        EpgIdOutputCase::LowercaseAscii => with_folded_epg_id(id, insert),
    }
}

async fn collect_referenced_ids<R, S>(mut records: S, case: EpgIdOutputCase) -> Result<HashSet<Arc<str>>, TuliproxError>
where
    R: StoredPlaylistRecord,
    S: Stream<Item = Result<R, TuliproxError>> + Unpin,
{
    let mut ids = HashSet::new();
    while let Some(entry) = records.next().await {
        let record = entry?;
        if let Some(id) = record.epg_channel_id().map(str::trim).filter(|id| !id.is_empty()) {
            insert_canonical_id(&mut ids, id, case);
        }
    }
    Ok(ids)
}

/// EPG ids the target's live playlist refers to, canonicalized like the EPG keys; `None` without a stored playlist.
async fn load_referenced_epg_ids(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    output: TargetType,
) -> Result<Option<HashSet<Arc<str>>>, InspectError> {
    let case = target_epg_case(target);
    let app_config = &app_state.app_config;
    let ids = match output {
        TargetType::Xtream => match iter_raw_xtream_target_playlist(app_config, target, XtreamCluster::Live).await {
            Some(records) => collect_referenced_ids(records, case).await,
            None => return Ok(None),
        },
        _ => match iter_raw_m3u_target_playlist(app_config, target, Some(XtreamCluster::Live)).await {
            Some(records) => collect_referenced_ids(records, case).await,
            None => return Ok(None),
        },
    };
    ids.map(Some).map_err(|err| {
        error!("Failed to read the stored playlist of target '{}': {err}", target.name);
        InspectError::internal("Failed to read the stored playlist database; see the server log")
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn require_target(value: Option<String>) -> Result<String, InspectError> {
    non_empty(value).ok_or_else(|| InspectError::bad_request("Missing target"))
}

fn parse_output(value: Option<&str>) -> Result<Option<EpgOutput>, InspectError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| parse_enum::<EpgOutput>("output", value))
        .transpose()
}

fn lookup_target(app_state: &AppState, name: &str) -> Result<Arc<ConfigTarget>, InspectError> {
    find_target_by_name(&app_state.app_config, name)
        .ok_or_else(|| InspectError::not_found(format!("Unknown target '{name}'")))
}

fn epg_failure(target: &str, err: &TuliproxError) -> InspectError {
    error!("Failed to read the EPG database of target '{target}': {err}");
    InspectError::internal("Failed to read the EPG database; see the server log")
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct EpgChannelsQuery {
    target: Option<String>,
    output: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    search: Option<String>,
    issue: Option<Vec<String>>,
}

#[derive(Serialize)]
struct EpgChannelsResponse<'a> {
    target: &'a str,
    output: Option<&'static str>,
    total_matched: usize,
    offset: usize,
    limit: usize,
    limit_clamped: bool,
    returned: usize,
    has_more: bool,
    channels: Vec<EpgChannelRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Default)]
struct ChannelsPage {
    total_matched: usize,
    channels: Vec<ChannelSummary>,
}

async fn run_epg_channels_query(
    app_state: &Arc<AppState>,
    query: EpgChannelsQuery,
) -> Result<serde_json::Value, InspectError> {
    let target_name = require_target(query.target)?;
    let output = parse_output(query.output.as_deref())?;
    let issues = parse_enum_list::<EpgIssue>("issue", query.issue.as_deref())?;
    let window = PageWindow::resolve(query.offset, query.limit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT);
    let needle = CaseInsensitiveNeedle::new(query.search.as_deref());

    let target = lookup_target(app_state, &target_name)?;
    let db = resolve_epg_db(&app_state.app_config.config.load(), &target, output)?;

    let (page, note) = match &db {
        EpgDb::Available(db_output, epg_path) => {
            let referenced = load_referenced_epg_ids(app_state, &target, *db_output).await?;
            let note = referenced.is_none().then(|| {
                format!(
                    "The playlist of target '{target_name}' is not stored, so referenced_by_playlist is unknown (null) \
                     and the unreferenced filter matches nothing."
                )
            });
            let now = Utc::now().timestamp();
            let page = scan_epg_db(
                &app_state.app_config.file_locks,
                epg_path,
                ChannelsPage::default(),
                move |page: &mut ChannelsPage, key, channel| {
                    let is_referenced = referenced.as_ref().map(|ids| ids.contains(&key));
                    let summary = summarize(key, &channel, now, is_referenced);
                    if !summary.matches_search(needle.as_ref()) || !summary.has_any_issue(&issues, now) {
                        return;
                    }
                    if window.contains(page.total_matched) {
                        page.channels.push(summary);
                    }
                    page.total_matched += 1;
                },
            )
            .await
            .map_err(|err| epg_failure(&target_name, &err))?;
            (page, note)
        }
        EpgDb::NoEpgOutput | EpgDb::Missing(_) => (ChannelsPage::default(), db.missing_note(&target_name)),
    };

    let returned = page.channels.len();
    let response = EpgChannelsResponse {
        target: &target_name,
        output: db.output(),
        total_matched: page.total_matched,
        offset: window.offset,
        limit: window.limit,
        limit_clamped: window.limit_clamped,
        returned,
        has_more: window.has_more(page.total_matched, returned),
        channels: page.channels.iter().map(EpgChannelRow::from).collect(),
        note,
    };
    serde_json::to_value(response).map_err(|err| InspectError::internal(format!("Failed to encode response: {err}")))
}

/// `GET /playlist/epg/channels`: channels of a target's EPG database, optionally filtered for processing problems.
pub(super) async fn query_target_epg_channels(
    State(app_state): State<Arc<AppState>>,
    FormQuery(query): FormQuery<EpgChannelsQuery>,
) -> axum::response::Response {
    match run_epg_channels_query(&app_state, query).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(err) => err.into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct EpgChannelQuery {
    target: Option<String>,
    output: Option<String>,
    channel_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<u32>,
}

fn parse_instant(param: &str, value: &str) -> Result<i64, InspectError> {
    DateTime::parse_from_rfc3339(value.trim()).map(|time| time.timestamp()).map_err(|_| {
        InspectError::bad_request(format!(
            "Invalid {param}; use RFC 3339, e.g. 2026-10-23T12:00:00Z (encode '+' in an offset as %2B)"
        ))
    })
}

/// `[from, to)` as unix seconds; `from` defaults to `now` and `to` to 24 hours after `from`.
fn parse_window(from: Option<&str>, to: Option<&str>, now: i64) -> Result<(i64, i64), InspectError> {
    let from_ts = from.map(str::trim).filter(|v| !v.is_empty()).map(|v| parse_instant("from", v)).transpose()?;
    let to_ts = to.map(str::trim).filter(|v| !v.is_empty()).map(|v| parse_instant("to", v)).transpose()?;
    let from_ts = from_ts.unwrap_or(now);
    let to_ts = to_ts.unwrap_or_else(|| from_ts.saturating_add(DEFAULT_WINDOW_SECS));
    if to_ts <= from_ts {
        return Err(InspectError::bad_request("'to' must be after 'from'"));
    }
    Ok((from_ts, to_ts))
}

#[derive(Serialize)]
struct ProgrammeRow {
    start: Option<String>,
    stop: Option<String>,
    title: Option<String>,
    desc: Option<String>,
    categories: Vec<String>,
    is_live: bool,
    is_new: bool,
}

impl From<&EpgProgramme> for ProgrammeRow {
    fn from(programme: &EpgProgramme) -> Self {
        Self {
            start: format_rfc3339(programme.start),
            stop: format_rfc3339(programme.stop),
            title: programme.title.as_deref().map(str::to_string),
            desc: programme.desc.as_deref().map(|desc| truncate_chars(desc, MAX_PROGRAMME_DESC_CHARS)),
            categories: programme.categories.iter().map(|category| category.value.to_string()).collect(),
            is_live: programme.is_live,
            is_new: programme.is_new,
        }
    }
}

/// Programmes overlapping `[from, to)`: how many there are and the first `limit` of them.
fn programmes_in_window(channel: &EpgChannel, from: i64, to: i64, limit: usize) -> (usize, Vec<ProgrammeRow>) {
    let mut total = 0_usize;
    let mut rows = Vec::new();
    for programme in channel.programmes.iter().filter(|programme| programme.stop > from && programme.start < to) {
        if rows.len() < limit {
            rows.push(ProgrammeRow::from(programme));
        }
        total += 1;
    }
    (total, rows)
}

#[derive(Serialize)]
struct EpgChannelResponse<'a> {
    target: &'a str,
    output: Option<&'static str>,
    channel: Option<EpgChannelMeta>,
    from: Option<String>,
    to: Option<String>,
    limit: usize,
    limit_clamped: bool,
    total_in_window: usize,
    returned: usize,
    truncated: bool,
    programmes: Vec<ProgrammeRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

async fn run_epg_channel_query(
    app_state: &Arc<AppState>,
    query: EpgChannelQuery,
) -> Result<serde_json::Value, InspectError> {
    let target_name = require_target(query.target)?;
    let output = parse_output(query.output.as_deref())?;
    let channel_id = non_empty(query.channel_id).ok_or_else(|| InspectError::bad_request("Missing channel_id"))?;
    let now = Utc::now().timestamp();
    let (from, to) = parse_window(query.from.as_deref(), query.to.as_deref(), now)?;
    let limits = PageWindow::resolve(None, query.limit, DEFAULT_PROGRAMME_LIMIT, MAX_PROGRAMME_LIMIT);

    let target = lookup_target(app_state, &target_name)?;
    let db = resolve_epg_db(&app_state.app_config.config.load(), &target, output)?;
    let mut response = EpgChannelResponse {
        target: &target_name,
        output: db.output(),
        channel: None,
        from: format_rfc3339(from),
        to: format_rfc3339(to),
        limit: limits.limit,
        limit_clamped: limits.limit_clamped,
        total_in_window: 0,
        returned: 0,
        truncated: false,
        programmes: Vec::new(),
        note: db.missing_note(&target_name),
    };

    if let EpgDb::Available(_, epg_path) = &db {
        let storage_key = canonicalize_untrusted_epg_id(&channel_id, target_epg_case(&target));
        let found = epg_query_channels_by_storage_key(
            &app_state.app_config.file_locks,
            epg_path,
            vec![Arc::clone(&storage_key)],
        )
        .await
        .map_err(|err| epg_failure(&target_name, &err))?;
        let Some((storage_key, Some(channel))) = found.into_iter().next() else {
            return Err(InspectError::not_found(format!(
                "EPG channel '{channel_id}' not found in the EPG database of target '{target_name}' \
                 (looked up as '{storage_key}'; list ids with /playlist/epg/channels)"
            )));
        };
        let (total_in_window, programmes) = programmes_in_window(&channel, from, to, limits.limit);
        response.channel = Some(EpgChannelMeta::from(&summarize(storage_key, &channel, now, None)));
        response.total_in_window = total_in_window;
        response.returned = programmes.len();
        response.truncated = total_in_window > programmes.len();
        response.programmes = programmes;
    }
    serde_json::to_value(response).map_err(|err| InspectError::internal(format!("Failed to encode response: {err}")))
}

/// `GET /playlist/epg/channel`: metadata and programmes of one channel of a target's EPG database.
pub(super) async fn get_target_epg_channel(
    State(app_state): State<Arc<AppState>>,
    FormQuery(query): FormQuery<EpgChannelQuery>,
) -> axum::response::Response {
    match run_epg_channel_query(&app_state, query).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests;
