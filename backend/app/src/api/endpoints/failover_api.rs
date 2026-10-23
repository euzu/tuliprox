use crate::api::{
    api_utils::failover_group_hot_apply::enabled_failover_target, auth_middleware::permission_layer, model::AppState,
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use regex::Regex;
use serde::Serialize;
use shared::model::{
    permission::Permission, CandidateDto, CandidatePageDto, ChannelMemberRefDto, FailoverCandidateFilter,
    FailoverGroupDto, FailoverGroupHealthDto, FailoverGroupListDto, FailoverGroupMemberDto, FailoverGroupOrigin,
    FailoverGroupTargetStatusDto, FailoverGroupTargetsDto, FailoverGroupsConfig, FailoverTargetState, GroupCapacityDto,
    MemberHealthDto, OverrideValidationError, TargetFailoverOverridesDto, TargetOverridesResponseDto, VirtualId,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tuliprox_core::model::{ConfigTarget, TargetOverridesEntry};
use tuliprox_processing::processor::failover_group::{
    derive_group_metadata, resolve_failover_groups, CatalogEntry, FailoverGroupResolution, MemberMetadata,
    ResolvedCandidate,
};
use tuliprox_repository::{run_blocking, CatalogRecord, FailoverGroupRecord, FailoverGroupTargetData};
use tuliprox_session::failover_group::{rank_members, CircuitView, SelectionMode};

const DEFAULT_PAGE_LIMIT: usize = 100;
const MAX_PAGE_LIMIT: usize = 500;
/// A route failure that is already the HTTP answer.
type ApiResult<T> = Result<T, ApiError>;

/// Boxed so the `Err` side of every route result stays small.
struct ApiError(Box<Response>);

const NO_CATALOG_MESSAGE: &str = "run a playlist update first";

pub fn failover_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let read_routes = read_routes().layer(permission_layer!(app_state, Permission::PlaylistRead));
    let write_routes = write::write_routes().layer(permission_layer!(app_state, Permission::PlaylistWrite));
    Router::new().merge(read_routes).merge(write_routes)
}

pub fn failover_api_register_unprotected() -> Router<Arc<AppState>> {
    Router::new().merge(read_routes()).merge(write::write_routes())
}

fn read_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/failover/targets", get(get_targets))
        .route("/failover/groups", get(get_groups))
        .route("/failover/overrides", get(get_overrides))
        .route("/failover/preview", post(post_preview))
        .route("/failover/candidates", get(get_candidates))
        .route("/failover/health", get(get_health))
}

#[derive(serde::Deserialize)]
struct TargetQuery {
    target: String,
}

#[derive(serde::Deserialize)]
struct CandidateQuery {
    target: String,
    q: Option<String>,
    /// `q` is a regular expression instead of a case-insensitive substring.
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    filter: FailoverCandidateFilter,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
struct HealthQuery {
    target: String,
    /// Virtual id of the group.
    group: u32,
}

fn error_response(status: StatusCode, message: impl std::fmt::Display) -> ApiError {
    ApiError(Box::new((status, Json(serde_json::json!({ "error": message.to_string() }))).into_response()))
}

/// An error answer with a structured body instead of the plain `{"error": ...}` message.
fn json_error(status: StatusCode, body: &impl Serialize) -> ApiError {
    ApiError(Box::new((status, Json(body)).into_response()))
}

fn respond<T: Serialize>(result: ApiResult<T>) -> Response { respond_with(StatusCode::OK, result) }

fn respond_with<T: Serialize>(status: StatusCode, result: ApiResult<T>) -> Response {
    let mut response = match result {
        Ok(value) => (status, Json(value)).into_response(),
        Err(ApiError(response)) => *response,
    };
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `(offset, limit)` of a page; the limit defaults to 100 and never exceeds 500.
fn page_window(offset: Option<usize>, limit: Option<usize>) -> (usize, usize) {
    (offset.unwrap_or(0), limit.unwrap_or(DEFAULT_PAGE_LIMIT).min(MAX_PAGE_LIMIT))
}

fn enabled_target(state: &AppState, name: &str) -> ApiResult<(Arc<ConfigTarget>, FailoverGroupsConfig)> {
    enabled_failover_target(state, name).ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, format!("Target '{name}' not found or failover groups are disabled"))
    })
}

async fn load_stored(state: &AppState, target: &ConfigTarget) -> Arc<FailoverGroupTargetData> {
    state.playlists.failover_groups.get_or_load(&state.app_config, &state.playlists, target).await
}

/// The cached catalog; an empty one means no playlist update has published it yet.
async fn load_catalog(state: &AppState, target: &ConfigTarget) -> ApiResult<Arc<[CatalogRecord]>> {
    match state.playlists.failover_groups.get_or_load_catalog(&state.app_config, &state.playlists, target).await {
        Ok(catalog) if catalog.is_empty() => Err(error_response(StatusCode::CONFLICT, NO_CATALOG_MESSAGE)),
        Ok(catalog) => Ok(catalog),
        Err(err) => Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, err)),
    }
}

/// Runs CPU or file bound work off the async runtime.
async fn run_off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    run_blocking(work).await.map_err(|err| error_response(StatusCode::INTERNAL_SERVER_ERROR, err))
}

fn path_string(path: &std::path::Path) -> String { path.to_string_lossy().into_owned() }

fn section_status(
    name: &str,
    section: Option<&TargetOverridesEntry>,
    stored: Option<&FailoverGroupTargetData>,
    state: FailoverTargetState,
) -> FailoverGroupTargetStatusDto {
    let findings: Vec<OverrideValidationError> = section.map(|entry| entry.findings.clone()).unwrap_or_default();
    let unresolved_groups = stored.map_or(0, |data| data.groups.values().filter(|g| !g.orphaned.is_empty()).count());
    FailoverGroupTargetStatusDto {
        name: name.to_string(),
        etag: section.map(|entry| entry.etag.to_string()),
        applied_etag: stored.and_then(|data| data.meta.overrides_etag.as_deref()).map(str::to_string),
        file: section.map(|entry| path_string(&entry.file)),
        group_count: stored.map_or(0, |data| data.groups.len()),
        problem_count: findings.len() + unresolved_groups + usize::from(state != FailoverTargetState::Enabled),
        findings,
        state,
    }
}

async fn get_targets(State(state): State<Arc<AppState>>) -> Response {
    let overrides = state.app_config.failover.load_full();
    let sources = state.app_config.sources.load();
    let configured: Vec<&Arc<ConfigTarget>> = sources.sources.iter().flat_map(|source| &source.targets).collect();
    let enabled: Vec<&Arc<ConfigTarget>> =
        configured.iter().copied().filter(|target| target.execution_plan.failover_groups.is_some()).collect();

    let mut targets = Vec::with_capacity(enabled.len() + overrides.sections.len());
    for target in enabled {
        let stored = load_stored(&state, target).await;
        let section = overrides.sections.get(target.name.as_str());
        targets.push(section_status(
            &target.name,
            section.map(AsRef::as_ref),
            Some(&stored),
            FailoverTargetState::Enabled,
        ));
    }
    // Sections nothing can process: the target is configured without failover groups, or does not exist.
    let mut unprocessed: Vec<(&Arc<str>, &Arc<TargetOverridesEntry>, FailoverTargetState)> = overrides
        .sections
        .iter()
        .filter_map(|(name, entry)| {
            let state = match configured.iter().find(|target| target.name == **name) {
                Some(target) if target.execution_plan.failover_groups.is_some() => return None,
                Some(_) => FailoverTargetState::Disabled,
                None => FailoverTargetState::Orphan,
            };
            Some((name, entry, state))
        })
        .collect();
    unprocessed.sort_by(|left, right| left.0.cmp(right.0));
    targets.extend(unprocessed.into_iter().map(|(name, entry, state)| section_status(name, Some(entry), None, state)));

    respond(Ok(FailoverGroupTargetsDto {
        source: overrides.source.as_ref().map(|source| path_string(source.path())),
        files_used: overrides.files_used.iter().map(|path| path_string(path)).collect(),
        load_error: overrides.error.clone(),
        targets,
    }))
}

async fn get_overrides(State(state): State<Arc<AppState>>, Query(query): Query<TargetQuery>) -> Response {
    respond(overrides_response(&state, &query.target))
}

fn overrides_response(state: &AppState, target_name: &str) -> ApiResult<TargetOverridesResponseDto> {
    let (target, _) = enabled_target(state, target_name)?;
    Ok(match state.app_config.failover_section(&target.name) {
        Some(entry) => TargetOverridesResponseDto {
            etag: Some(entry.etag.to_string()),
            file: Some(path_string(&entry.file)),
            overrides: entry.overrides.clone(),
        },
        None => TargetOverridesResponseDto {
            etag: None,
            file: None,
            overrides: TargetFailoverOverridesDto::new(&target.name),
        },
    })
}

/// Persisted groups by `(manual, key)`: manual ids and auto keys are separate namespaces.
fn stored_by_key(stored: &FailoverGroupTargetData) -> HashMap<(bool, &str), &FailoverGroupRecord> {
    stored
        .groups
        .values()
        .map(|record| ((record.origin == FailoverGroupOrigin::Manual, record.key.as_ref()), record.as_ref()))
        .collect()
}

fn group_dto(
    config: &FailoverGroupsConfig,
    candidate: ResolvedCandidate,
    catalog: &[CatalogRecord],
    stored: &HashMap<(bool, &str), &FailoverGroupRecord>,
) -> FailoverGroupDto {
    let members: Vec<&CatalogRecord> = candidate.entries.iter().filter_map(|&entry| catalog.get(entry)).collect();
    let metadata = derive_group_metadata(
        &candidate,
        members.iter().map(|member| MemberMetadata {
            name: &member.name,
            title: &member.title,
            logo: &member.logo,
            epg_channel_id: member.epg_channel_id.as_deref(),
            playlist_group: &member.playlist_group,
        }),
        config.name_strip_quality,
    );
    let ResolvedCandidate { key, origin, policy, unresolved, .. } = candidate;
    let origin = FailoverGroupOrigin::from(origin);
    let record = stored.get(&(origin == FailoverGroupOrigin::Manual, key.as_str())).copied();

    let members = members
        .into_iter()
        .map(|member| FailoverGroupMemberDto {
            member_vid: record.and_then(|record| record.member_vid(&member.member_ref)),
            member_ref: member.member_ref.clone(),
            title: member.title.to_string(),
            input_name: member.member_ref.input.clone(),
            provider_id: member.provider_id,
            has_archive: member.has_archive,
        })
        .collect();

    FailoverGroupDto {
        group_vid: record.map(|record| record.group_vid),
        key,
        origin,
        policy,
        name: metadata.name,
        logo: Some(metadata.logo).filter(|logo| !logo.is_empty()),
        epg_channel_id: metadata.epg_channel_id,
        playlist_group: metadata.playlist_group,
        members,
        unresolved,
    }
}

/// Resolves the catalog with `overrides` exactly as processing does and maps the result to DTOs.
fn resolve_group_list(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    catalog: &[CatalogRecord],
    stored: &FailoverGroupTargetData,
) -> (Vec<FailoverGroupDto>, shared::model::FailoverOverrideDiagnosticsDto) {
    let entries: Vec<CatalogEntry<'_>> = catalog
        .iter()
        .map(|record| CatalogEntry {
            member_ref: &record.member_ref,
            natural_key: record.natural_key.as_deref(),
            dsl_forced: record.dsl_forced,
            epg_channel_id: record.epg_channel_id.as_deref(),
        })
        .collect();
    let FailoverGroupResolution { candidates, diagnostics } = resolve_failover_groups(config, overrides, &entries);
    let stored = stored_by_key(stored);
    let groups = candidates.into_iter().map(|candidate| group_dto(config, candidate, catalog, &stored)).collect();
    (groups, diagnostics)
}

async fn get_groups(State(state): State<Arc<AppState>>, Query(query): Query<TargetQuery>) -> Response {
    respond(groups_response(&state, &query.target).await)
}

/// The persisted state: the catalog resolved with the section the last playlist update applied.
async fn groups_response(state: &AppState, target_name: &str) -> ApiResult<FailoverGroupListDto> {
    let (target, config) = enabled_target(state, target_name)?;
    let catalog = load_catalog(state, &target).await?;
    let stored = load_stored(state, &target).await;
    let etag = state.app_config.failover_section(&target.name).map(|entry| entry.etag.to_string());
    let applied_etag = stored.meta.overrides_etag.as_deref().map(str::to_string);
    let display_stale = stored.meta.display_stale_groups.clone();
    run_off_runtime(move || {
        let (groups, diagnostics) =
            resolve_group_list(&config, stored.meta.applied_overrides.as_ref(), &catalog, &stored);
        FailoverGroupListDto { etag, applied_etag, groups, diagnostics, display_stale }
    })
    .await
}

async fn post_preview(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TargetQuery>,
    Json(draft): Json<TargetFailoverOverridesDto>,
) -> Response {
    respond(preview_response(&state, &query.target, draft).await)
}

/// The groups a draft section would produce; nothing is written.
async fn preview_response(
    state: &AppState,
    target_name: &str,
    mut draft: TargetFailoverOverridesDto,
) -> ApiResult<FailoverGroupListDto> {
    let (target, config) = enabled_target(state, target_name)?;
    let catalog = load_catalog(state, &target).await?;
    let stored = load_stored(state, &target).await;
    draft.target.clone_from(&target.name);
    draft.canonicalize();
    let etag = state.app_config.failover_section(&target.name).map(|entry| entry.etag.to_string());
    let applied_etag = stored.meta.overrides_etag.as_deref().map(str::to_string);
    let display_stale = stored.meta.display_stale_groups.clone();
    run_off_runtime(move || {
        let (groups, diagnostics) = resolve_group_list(&config, Some(&draft), &catalog, &stored);
        FailoverGroupListDto { etag, applied_etag, groups, diagnostics, display_stale }
    })
    .await
}

/// Case-insensitive substring test; `needle_lower` is already lowercase.
fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let needle = needle_lower.as_bytes();
        return haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle));
    }
    haystack.to_lowercase().contains(needle_lower)
}

/// Title test of a candidate search.
enum TitleMatcher {
    /// Case-insensitive substring; the needle is already lowercase.
    Text(String),
    Regex(Regex),
}

impl TitleMatcher {
    fn new(query: &str, regex: bool) -> Result<Self, regex::Error> {
        if regex && !query.is_empty() {
            Regex::new(query).map(Self::Regex)
        } else {
            Ok(Self::Text(query.trim().to_lowercase()))
        }
    }

    fn matches(&self, title: &str) -> bool {
        match self {
            Self::Text(needle) => contains_ignore_case(title, needle),
            Self::Regex(regex) => regex.is_match(title),
        }
    }
}

fn search_catalog(
    catalog: &[CatalogRecord],
    stored: &FailoverGroupTargetData,
    excluded: &[ChannelMemberRefDto],
    filter: FailoverCandidateFilter,
    matcher: &TitleMatcher,
    (offset, limit): (usize, usize),
) -> CandidatePageDto {
    let excluded: HashSet<&ChannelMemberRefDto> = excluded.iter().collect();
    let group_of: HashMap<(&str, &str), &str> = stored
        .groups
        .values()
        .flat_map(|group| {
            group
                .members
                .iter()
                .map(|member| ((member.input_name.as_ref(), member.input_stream_id.as_ref()), group.key.as_ref()))
        })
        .collect();

    let mut total = 0;
    let mut items = Vec::with_capacity(limit.min(catalog.len()));
    for record in catalog {
        let group_key =
            group_of.get(&(record.member_ref.input.as_str(), record.member_ref.stream_id.as_str())).copied();
        let is_excluded = excluded.contains(&record.member_ref);
        let selected = match filter {
            FailoverCandidateFilter::All => true,
            FailoverCandidateFilter::Ungrouped => group_key.is_none(),
            FailoverCandidateFilter::Grouped => group_key.is_some(),
            FailoverCandidateFilter::Excluded => is_excluded,
        };
        if !selected || !matcher.matches(&record.title) {
            continue;
        }
        if total >= offset && items.len() < limit {
            items.push(CandidateDto {
                member_ref: record.member_ref.clone(),
                title: record.title.to_string(),
                logo: record.logo.to_string(),
                playlist_group: record.playlist_group.to_string(),
                provider_id: record.provider_id,
                has_archive: record.has_archive,
                group_key: group_key.map(str::to_string),
                excluded: is_excluded,
            });
        }
        total += 1;
    }
    CandidatePageDto { total, items }
}

async fn get_candidates(State(state): State<Arc<AppState>>, Query(query): Query<CandidateQuery>) -> Response {
    respond(candidates_response(&state, query).await)
}

/// Channels of the catalog with their persisted group; `excluded` reflects the current failover files.
async fn candidates_response(state: &AppState, query: CandidateQuery) -> ApiResult<CandidatePageDto> {
    let (target, _) = enabled_target(state, &query.target)?;
    let catalog = load_catalog(state, &target).await?;
    let stored = load_stored(state, &target).await;
    let section = state.app_config.failover_section(&target.name);
    let window = page_window(query.offset, query.limit);
    let matcher = TitleMatcher::new(query.q.as_deref().unwrap_or_default(), query.regex)
        .map_err(|err| error_response(StatusCode::BAD_REQUEST, format!("Invalid regular expression: {err}")))?;
    run_off_runtime(move || {
        let excluded = section.as_ref().map_or(&[][..], |entry| entry.overrides.excluded.as_slice());
        search_catalog(&catalog, &stored, excluded, query.filter, &matcher, window)
    })
    .await
}

fn circuit_label(view: CircuitView) -> (&'static str, Option<u64>) {
    match view {
        CircuitView::Closed => ("closed", None),
        CircuitView::Counting(_) => ("counting", None),
        CircuitView::Open { remaining } => ("open", Some(remaining.as_secs())),
        CircuitView::HalfOpenInFlight => ("half_open", None),
    }
}

async fn get_health(State(state): State<Arc<AppState>>, Query(query): Query<HealthQuery>) -> Response {
    respond(health_response(&state, &query).await)
}

/// Member health of one group from the in-memory indexes; no provider is contacted.
async fn health_response(state: &AppState, query: &HealthQuery) -> ApiResult<FailoverGroupHealthDto> {
    let (target, _) = enabled_target(state, &query.target)?;
    let stored = load_stored(state, &target).await;
    let Some(group) = stored.groups.get(&VirtualId::new(query.group)) else {
        return Err(error_response(StatusCode::NOT_FOUND, format!("Failover group {} not found", query.group)));
    };

    let target_name: Arc<str> = Arc::from(target.name.as_str());
    let now = Instant::now();
    let health = &state.member_health;
    let ranked = rank_members(&target_name, &group.members, group.policy, health, &SelectionMode::Live, now);

    let mut accounts: HashMap<Arc<str>, (usize, usize)> = HashMap::new();
    let mut members = Vec::with_capacity(ranked.len());
    for (position, candidate) in ranked.into_iter().enumerate() {
        let Some(member) = group.members.get(candidate.index) else {
            continue;
        };
        let member_health = health.health(&target_name, member, now);
        let (circuit, circuit_remaining_secs) = circuit_label(member_health.circuit);
        let (input_circuit, _) = circuit_label(health.input_circuit_view(&member.input_name, now));

        let (mut slots_used, mut slots_max) = (0, 0);
        for (account, used, max) in state.active_provider.provider_capacities_for_input(&member.input_name) {
            slots_used += used;
            slots_max += max;
            accounts.insert(account, (used, max));
        }

        members.push(MemberHealthDto {
            member_vid: member.member_vid,
            member_ref: ChannelMemberRefDto {
                input: member.input_name.to_string(),
                stream_id: member.input_stream_id.to_string(),
            },
            title: member.title.to_string(),
            rank: position + 1,
            qos_24h: member_health.qos.map(|qos| qos.score_24h),
            qos_7d: member_health.qos.map(|qos| qos.score_7d),
            confidence: member_health.qos.map(|qos| qos.confidence),
            failure_streak: member_health.qos.map_or(0, |qos| qos.failure_streak),
            resolution: member_health.probe.as_ref().map(|probe| probe.resolution.clone()),
            codec: member_health.probe.as_ref().map(|probe| probe.codec.clone()),
            bitrate_kbps: member_health.probe.as_ref().map(|probe| probe.bitrate_kbps),
            circuit: circuit.to_string(),
            circuit_remaining_secs,
            input_circuit: input_circuit.to_string(),
            slots_used: u32::try_from(slots_used).unwrap_or(u32::MAX),
            slots_max: u32::try_from(slots_max).unwrap_or(u32::MAX),
            score: candidate.score,
        });
    }

    let capacity = accounts.values().fold(GroupCapacityDto::default(), |mut capacity, &(used, max)| {
        capacity.accounts += 1;
        capacity.in_use = capacity.in_use.saturating_add(u32::try_from(used).unwrap_or(u32::MAX));
        capacity.max_connections = capacity.max_connections.saturating_add(u32::try_from(max).unwrap_or(u32::MAX));
        capacity
    });
    Ok(FailoverGroupHealthDto { members, capacity })
}

mod write;

#[cfg(test)]
mod tests;
