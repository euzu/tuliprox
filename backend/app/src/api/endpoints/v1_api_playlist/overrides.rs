//! Manual per-item playlist overrides: reading and saving the override files, item provenance,
//! orphan detection, discard cleanup and the EPG channel search of the Explorer.
//!
//! The browser never uploads a whole override file. A save carries field-level tagged operations
//! for one or more items and the revision it was based on; the repository applies them under a
//! per-file lock and rejects a save whose base revision is outdated. Reads and writes go through
//! the one repository and status store the [`AppState`] holds, so those locks hold across
//! requests.
//!
//! Every input/target name is resolved the way the other playlist endpoints resolve them: a name
//! that is not configured answers `404`, exactly like every other name that is not visible to the
//! caller. Nothing distinguishes an unknown name from a hidden one.

use super::{
    epg::load_cached_epg_channels_for_input, epg_inspect::with_epg_query, inspect_common::non_empty,
    items::looks_credential_bearing, recording::find_target_by_name,
};
use crate::{
    api::{
        endpoints::xmltv_api::get_epg_path_for_target_by_type,
        model::{check_scan_cancelled, AppState},
    },
    model::{ConfigInput, ConfigTarget},
    repository::{
        iter_raw_m3u_input_playlist, iter_raw_m3u_target_playlist, iter_raw_xtream_input_playlist,
        iter_raw_xtream_target_playlist, LoadedOverrides, LockedReceiverStream, OverrideParseResult, OverridePatchItem,
        OverrideRevision, OverrideScope, OverrideWriteError, ParsedOverrideFile,
    },
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::{error, warn};
use serde::Deserialize;
use serde_json::Value;
use shared::{
    error::TuliproxError,
    model::{
        EpgChannel, EpgSearchResultDto, FieldError, InputOverrideSections, ItemFieldsDto, ItemOverride,
        ItemOverridePatch, ItemPatchDto, M3uPlaylistItem, OrphanedOverrideFileDto, OverrideConflictDto,
        OverrideDiscardRequest, OverrideEntryDto, OverrideEntryState, OverrideFieldErrorsDto, OverrideFieldOp,
        OverrideFileDto, OverrideItemKind, OverrideOrphanEntryDto, OverrideOrphansDto, OverrideProvenanceDto,
        OverrideRevisionDto, OverrideSaveRequest, OverrideScopeDto, PlaylistItemHeader, PlaylistItemType, TargetType,
        XtreamCluster, XtreamPlaylistItem,
    },
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use tokio_stream::StreamExt;
use tuliprox_core::model::{override_key, InputAliasMap, OverrideKey};

/// Default number of EPG channel candidates one search returns.
const EPG_SEARCH_DEFAULT_LIMIT: u32 = 20;
/// Hard maximum of EPG channel candidates one search returns.
const EPG_SEARCH_MAX_LIMIT: u32 = 50;
/// Upper bound of matches one search collects before it stops collecting.
const EPG_SEARCH_MAX_MATCHES: usize = 500;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Everything the override endpoints answer with, rendered as the JSON body the Explorer expects.
#[derive(Debug)]
enum OverrideApiError {
    BadRequest(String),
    NotFound(String),
    /// `409`: the file changed since the request was based on it.
    Conflict {
        current_revision: String,
        message: Option<String>,
    },
    /// `422`: the patch is well-formed but not valid for the item kind.
    FieldErrors(Vec<FieldError>),
    Internal(String),
}

impl OverrideApiError {
    fn bad_request(message: impl Into<String>) -> Self { Self::BadRequest(message.into()) }

    fn not_found(message: impl Into<String>) -> Self { Self::NotFound(message.into()) }

    fn internal(message: impl Into<String>) -> Self { Self::Internal(message.into()) }
}

impl IntoResponse for OverrideApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(message) => {
                (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": message }))).into_response()
            }
            Self::NotFound(message) => {
                (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": message }))).into_response()
            }
            Self::Conflict { current_revision, message } => {
                (StatusCode::CONFLICT, axum::Json(OverrideConflictDto { current_revision, error: message }))
                    .into_response()
            }
            Self::FieldErrors(field_errors) => {
                (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(OverrideFieldErrorsDto { field_errors })).into_response()
            }
            Self::Internal(message) => {
                error!("{message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({ "error": "Internal error; see the server log" })),
                )
                    .into_response()
            }
        }
    }
}

fn ok_json<T: serde::Serialize>(value: T) -> Response { axum::Json(value).into_response() }

// ---------------------------------------------------------------------------------------------
// Scope resolution
// ---------------------------------------------------------------------------------------------

/// A file scope after its name has been checked against the configuration.
struct ResolvedScope {
    /// The file the request reads or writes.
    scope: OverrideScope,
    /// The configured input the file belongs to, for input scopes.
    input: Option<Arc<ConfigInput>>,
}

fn parse_scope_dto(value: &str) -> Result<OverrideScopeDto, OverrideApiError> {
    match value.trim() {
        "input" => Ok(OverrideScopeDto::Input),
        "target" => Ok(OverrideScopeDto::Target),
        other => {
            let shown: String = other.chars().take(32).collect();
            Err(OverrideApiError::bad_request(format!("Unknown scope '{shown}'; allowed values: input, target")))
        }
    }
}

fn scope_dto_of(scope: &OverrideScope) -> OverrideScopeDto {
    match scope {
        OverrideScope::Input(_) => OverrideScopeDto::Input,
        OverrideScope::Target(_) => OverrideScopeDto::Target,
    }
}

/// The parent input of `name`: override files are per configured input, not per runtime alias.
fn parent_input_name(aliases: &InputAliasMap, name: &str) -> String {
    aliases.parent_of(name).map_or_else(|| name.to_string(), ToString::to_string)
}

fn check_scope_name(name: &str) -> Result<&str, OverrideApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(OverrideApiError::bad_request("The 'name' parameter must not be empty"));
    }
    if trimmed != name {
        return Err(OverrideApiError::bad_request(format!(
            "The 'name' parameter must not have surrounding whitespace: '{name}'"
        )));
    }
    Ok(trimmed)
}

/// Resolves `name` for `scope_dto`; an unknown (or hidden) name answers `404`, like every other
/// playlist endpoint.
fn resolve_scope(
    app_state: &Arc<AppState>,
    scope_dto: OverrideScopeDto,
    name: &str,
) -> Result<ResolvedScope, OverrideApiError> {
    let name = check_scope_name(name)?;
    let sources = app_state.app_config.sources.load();
    match scope_dto {
        OverrideScopeDto::Input => {
            let input = sources
                .inputs
                .iter()
                .find(|input| input.name.as_ref() == name)
                .cloned()
                .ok_or_else(|| OverrideApiError::not_found(format!("Unknown input '{name}'")))?;
            let aliases = InputAliasMap::from_inputs(&sources.inputs);
            let parent = parent_input_name(&aliases, name);
            Ok(ResolvedScope { scope: OverrideScope::Input(parent), input: Some(input) })
        }
        OverrideScopeDto::Target => {
            let target = find_target_by_name(&app_state.app_config, name)
                .ok_or_else(|| OverrideApiError::not_found(format!("Unknown target '{name}'")))?;
            Ok(ResolvedScope { scope: OverrideScope::Target(target.name.clone()), input: None })
        }
    }
}

fn cluster_for(kind: OverrideItemKind) -> XtreamCluster {
    match kind {
        OverrideItemKind::Live => XtreamCluster::Live,
        OverrideItemKind::Vod => XtreamCluster::Video,
        OverrideItemKind::Series => XtreamCluster::Series,
    }
}

fn kind_name(kind: OverrideItemKind) -> &'static str {
    match kind {
        OverrideItemKind::Live => "live",
        OverrideItemKind::Vod => "vod",
        OverrideItemKind::Series => "series",
    }
}

fn parse_kind(value: Option<&str>) -> Result<OverrideItemKind, OverrideApiError> {
    match value.map(str::trim) {
        Some("live") => Ok(OverrideItemKind::Live),
        Some("vod") => Ok(OverrideItemKind::Vod),
        Some("series") => Ok(OverrideItemKind::Series),
        Some(other) => {
            let shown: String = other.chars().take(32).collect();
            Err(OverrideApiError::bad_request(format!("Unknown kind '{shown}'; allowed values: live, vod, series")))
        }
        None => Err(OverrideApiError::bad_request("Missing 'kind'; allowed values: live, vod, series")),
    }
}

// ---------------------------------------------------------------------------------------------
// Stored file representation
// ---------------------------------------------------------------------------------------------

fn section(sections: &InputOverrideSections, kind: OverrideItemKind) -> &BTreeMap<String, ItemOverride> {
    match kind {
        OverrideItemKind::Live => &sections.live,
        OverrideItemKind::Vod => &sections.vod,
        OverrideItemKind::Series => &sections.series,
    }
}

fn sections_of<'a>(file: &'a ParsedOverrideFile, input: Option<&str>) -> Option<&'a InputOverrideSections> {
    match file {
        ParsedOverrideFile::Input(file) => Some(&file.sections),
        ParsedOverrideFile::Target(file) => file.inputs.get(input?),
    }
}

fn stored_item<'a>(
    file: &'a ParsedOverrideFile,
    input: Option<&str>,
    kind: OverrideItemKind,
    id: &str,
) -> Option<&'a ItemOverride> {
    section(sections_of(file, input)?, kind).get(id)
}

/// Stored deltas as the wire form: every set field with its value, `null` for a cleared field.
fn fields_of(item: &ItemOverride) -> BTreeMap<String, Value> {
    match serde_json::to_value(item) {
        Ok(Value::Object(map)) => map.into_iter().filter(|(key, _)| key != "_hint").collect(),
        _ => BTreeMap::new(),
    }
}

fn push_entries(entries: &mut Vec<OverrideEntryDto>, input: Option<&str>, sections: &InputOverrideSections) {
    for kind in [OverrideItemKind::Live, OverrideItemKind::Vod, OverrideItemKind::Series] {
        for (id, item) in section(sections, kind) {
            entries.push(OverrideEntryDto {
                input: input.map(ToString::to_string),
                kind,
                id: id.clone(),
                hint: item.hint.clone(),
                fields: fields_of(item),
            });
        }
    }
}

fn entries_of(file: &ParsedOverrideFile) -> Vec<OverrideEntryDto> {
    let mut entries = Vec::new();
    match file {
        ParsedOverrideFile::Input(file) => push_entries(&mut entries, None, &file.sections),
        ParsedOverrideFile::Target(file) => {
            for (input_name, sections) in &file.inputs {
                push_entries(&mut entries, Some(input_name), sections);
            }
        }
    }
    entries
}

fn file_dto(loaded: LoadedOverrides) -> OverrideFileDto {
    let revision = loaded.revision.to_string();
    match loaded.parsed {
        OverrideParseResult::Valid { file, field_errors } => OverrideFileDto {
            revision,
            entries: entries_of(&file),
            field_errors,
            parse_error: None,
            schema_error: None,
        },
        OverrideParseResult::Malformed { error } => {
            OverrideFileDto { revision, parse_error: Some(error), ..OverrideFileDto::default() }
        }
        OverrideParseResult::UnsupportedSchema { version } => {
            OverrideFileDto { revision, schema_error: Some(version), ..OverrideFileDto::default() }
        }
    }
}

async fn load_scope(app_state: &Arc<AppState>, scope: &OverrideScope) -> Result<LoadedOverrides, OverrideApiError> {
    app_state.override_repository().load(scope).await.map_err(|err| {
        OverrideApiError::internal(format!("Failed to read the override file of '{}': {err}", scope.name()))
    })
}

// ---------------------------------------------------------------------------------------------
// Stored inventory
// ---------------------------------------------------------------------------------------------

/// One stored record, reduced to its override key and the fields the Explorer shows.
struct InventoryItem {
    key: OverrideKey,
    fields: ItemFieldsDto,
}

/// Result of scanning the stored playlists of one input.
#[derive(Default)]
struct InventoryScan {
    items: Vec<InventoryItem>,
    /// Records the reader could not decode; the scan is then not authoritative.
    skipped_records: usize,
    /// Every stored database of the input existed.
    complete: bool,
}

impl InventoryScan {
    /// Whether the scan may be used to call entries orphaned: a missing database or an unreadable
    /// record never marks anything as orphaned.
    fn is_authoritative(&self) -> bool { self.complete && self.skipped_records == 0 }

    fn keys(&self) -> HashSet<(OverrideItemKind, &str)> {
        self.items.iter().map(|item| (item.key.kind, item.key.id.as_ref())).collect()
    }
}

fn visible_text_opt(value: &str) -> Option<String> {
    (!value.is_empty() && !looks_credential_bearing(value)).then(|| value.to_string())
}

fn header_of(id: Arc<str>, url: &Arc<str>, input_name: &Arc<str>, item_type: PlaylistItemType) -> PlaylistItemHeader {
    PlaylistItemHeader {
        id,
        url: Arc::clone(url),
        input_name: Arc::clone(input_name),
        item_type,
        ..PlaylistItemHeader::default()
    }
}

fn xtream_inventory_item(item: &XtreamPlaylistItem, aliases: &InputAliasMap) -> Option<InventoryItem> {
    let header = header_of(Arc::from(item.provider_id.to_string()), &item.url, &item.input_name, item.item_type);
    let key = override_key(&header, aliases)?;
    Some(InventoryItem {
        key,
        fields: ItemFieldsDto {
            name: item.name.to_string(),
            title: item.title.to_string(),
            group: item.group.to_string(),
            logo: visible_text_opt(&item.logo),
            logo_small: visible_text_opt(&item.logo_small),
            epg_channel_id: item.epg_channel_id.as_ref().map(ToString::to_string),
            chno: item.channel_no,
            parent_code: item.parent_code.to_string(),
            audio_track: String::new(),
            time_shift: String::new(),
            rec: item.rec.to_string(),
        },
    })
}

fn m3u_inventory_item(item: &M3uPlaylistItem, aliases: &InputAliasMap) -> Option<InventoryItem> {
    let header = header_of(Arc::clone(&item.provider_id), &item.url, &item.input_name, item.item_type);
    let key = override_key(&header, aliases)?;
    Some(InventoryItem {
        key,
        fields: ItemFieldsDto {
            name: item.name.to_string(),
            title: item.title.to_string(),
            group: item.group.to_string(),
            logo: visible_text_opt(&item.logo),
            logo_small: visible_text_opt(&item.logo_small),
            epg_channel_id: item.epg_channel_id.as_ref().map(ToString::to_string),
            chno: item.chno,
            parent_code: item.parent_code.to_string(),
            audio_track: item.audio_track.to_string(),
            time_shift: item.time_shift.to_string(),
            rec: item.rec.to_string(),
        },
    })
}

fn read_failure(source: &str, name: &str, err: &TuliproxError) -> OverrideApiError {
    error!("Failed to read the stored playlist of {source} '{name}': {err}");
    OverrideApiError::internal("Failed to read the stored playlist database; see the server log")
}

async fn drain_inventory<R, F>(
    mut records: LockedReceiverStream<Result<R, TuliproxError>>,
    mut map: F,
    out: &mut Vec<InventoryItem>,
) -> Result<usize, TuliproxError>
where
    F: FnMut(&R) -> Option<InventoryItem>,
{
    while let Some(entry) = records.next().await {
        let record = entry?;
        if let Some(item) = map(&record) {
            out.push(item);
        }
    }
    Ok(records.skipped_records())
}

/// Streams the stored records of one cluster of an input, stopping as soon as `visit` returns `true`.
///
/// A missing database yields no records rather than an error: the caller is looking for data that may
/// simply not exist.
async fn scan_input_cluster<F>(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    cluster: XtreamCluster,
    aliases: &InputAliasMap,
    mut visit: F,
) -> Result<(), OverrideApiError>
where
    F: FnMut(InventoryItem) -> bool,
{
    let app_config = &app_state.app_config;
    if input.input_type.is_xtream() {
        let Some(mut records) = iter_raw_xtream_input_playlist(app_config, input, cluster).await else {
            return Ok(());
        };
        while let Some(entry) = records.next().await {
            let item = entry.map_err(|err| read_failure("input", &input.name, &err))?;
            if xtream_inventory_item(&item, aliases).is_some_and(&mut visit) {
                return Ok(());
            }
        }
    } else if input.input_type.is_m3u() {
        let Some(mut records) = iter_raw_m3u_input_playlist(app_config, input, Some(cluster)).await else {
            return Ok(());
        };
        while let Some(entry) = records.next().await {
            let item = entry.map_err(|err| read_failure("input", &input.name, &err))?;
            if m3u_inventory_item(&item, aliases).is_some_and(&mut visit) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The stored records of one input that carry `key`.
///
/// Only the cluster of the item kind is scanned, and the scan stops at the second match: a third
/// record cannot change the answer (the override key need not be unique), so the rest of the
/// database is never decoded. This keeps a single-item lookup from reading an entire provider.
async fn scan_input_item_matches(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    aliases: &InputAliasMap,
    key: &OverrideKey,
) -> Result<Vec<ItemFieldsDto>, OverrideApiError> {
    let mut matches: Vec<ItemFieldsDto> = Vec::new();
    scan_input_cluster(app_state, input, cluster_for(key.kind), aliases, |found| {
        if found.key != *key {
            return false;
        }
        matches.push(found.fields);
        matches.len() > 1
    })
    .await?;
    Ok(matches)
}

/// Scans the stored input playlists; `complete` stays `false` when a database is missing.
async fn scan_input_inventory(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    aliases: &InputAliasMap,
) -> Result<InventoryScan, OverrideApiError> {
    let app_config = &app_state.app_config;
    let mut scan = InventoryScan::default();

    if input.input_type.is_xtream() {
        let mut complete = true;
        for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            let Some(records) = iter_raw_xtream_input_playlist(app_config, input, cluster).await else {
                complete = false;
                continue;
            };
            let skipped = drain_inventory(
                records,
                |item: &XtreamPlaylistItem| xtream_inventory_item(item, aliases),
                &mut scan.items,
            )
            .await
            .map_err(|err| read_failure("input", &input.name, &err))?;
            scan.skipped_records += skipped;
        }
        scan.complete = complete;
    } else if input.input_type.is_m3u() {
        let Some(records) = iter_raw_m3u_input_playlist(app_config, input, None).await else {
            return Ok(scan);
        };
        scan.skipped_records =
            drain_inventory(records, |item: &M3uPlaylistItem| m3u_inventory_item(item, aliases), &mut scan.items)
                .await
                .map_err(|err| read_failure("input", &input.name, &err))?;
        scan.complete = true;
    }

    Ok(scan)
}

/// The record of a target that carries `kind`/`id`, if the target stores a playlist at all.
async fn scan_target_item(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    aliases: &InputAliasMap,
    kind: OverrideItemKind,
    id: &str,
) -> Result<Option<ItemFieldsDto>, OverrideApiError> {
    let key = OverrideKey::new(kind, Arc::<str>::from(id));
    let app_config = &app_state.app_config;
    let cluster = cluster_for(kind);
    if target.has_output(TargetType::Xtream) {
        let Some(mut records) = iter_raw_xtream_target_playlist(app_config, target, cluster).await else {
            return Ok(None);
        };
        while let Some(entry) = records.next().await {
            let item = entry.map_err(|err| read_failure("target", &target.name, &err))?;
            if let Some(found) = xtream_inventory_item(&item, aliases) {
                if found.key == key {
                    return Ok(Some(found.fields));
                }
            }
        }
    } else if target.has_output(TargetType::M3u) {
        let Some(mut records) = iter_raw_m3u_target_playlist(app_config, target, Some(cluster)).await else {
            return Ok(None);
        };
        while let Some(entry) = records.next().await {
            let item = entry.map_err(|err| read_failure("target", &target.name, &err))?;
            if let Some(found) = m3u_inventory_item(&item, aliases) {
                if found.key == key {
                    return Ok(Some(found.fields));
                }
            }
        }
    }
    Ok(None)
}

/// `(group / title)` of the raw input item.
fn hint_of(fields: &ItemFieldsDto) -> String { format!("{} / {}", fields.group.trim(), fields.title.trim()) }

/// Hints of one parent input, keyed by override key.
type HintIndex = HashMap<(OverrideItemKind, String), String>;

/// The keys a save is about to patch, per parent input.
type WantedHints = HashSet<(OverrideItemKind, String)>;

/// Hints for `wanted` only, from the clusters of their item kinds.
///
/// Only clusters that can hold a wanted key are read, and a cluster is abandoned as soon as every
/// wanted key of its kind is resolved, so a save never scans an inventory it does not need.
async fn hint_index(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    aliases: &InputAliasMap,
    wanted: &WantedHints,
) -> HintIndex {
    let mut index = HintIndex::new();
    let mut remaining = wanted.len();
    for kind in [OverrideItemKind::Live, OverrideItemKind::Vod, OverrideItemKind::Series] {
        if remaining == 0 {
            break;
        }
        if !wanted.iter().any(|(wanted_kind, _)| *wanted_kind == kind) {
            continue;
        }
        let result = scan_input_cluster(app_state, input, cluster_for(kind), aliases, |found| {
            let key = (found.key.kind, found.key.id.to_string());
            if wanted.contains(&key) && index.insert(key, hint_of(&found.fields)).is_none() {
                remaining -= 1;
            }
            remaining == 0
        })
        .await;
        if let Err(err) = result {
            // Hints are informational: a failed scan falls back to the hints the request carries.
            warn!("Falling back to request hints for input '{}': {err:?}", input.name);
            break;
        }
    }
    index
}

// ---------------------------------------------------------------------------------------------
// GET /playlist/overrides
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct OverrideFileQuery {
    scope: String,
    name: String,
}

async fn read_override_file(
    app_state: &Arc<AppState>,
    query: &OverrideFileQuery,
) -> Result<OverrideFileDto, OverrideApiError> {
    let scope_dto = parse_scope_dto(&query.scope)?;
    let resolved = resolve_scope(app_state, scope_dto, &query.name)?;
    let loaded = load_scope(app_state, &resolved.scope).await?;
    Ok(file_dto(loaded))
}

pub(super) async fn playlist_overrides_get(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<OverrideFileQuery>,
) -> Response {
    match read_override_file(&app_state, &query).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /playlist/overrides
// ---------------------------------------------------------------------------------------------

/// One request patch after its input has been normalized and its key checked for duplicates.
struct NormalizedPatch {
    /// Parent input; `None` inside an input file, which carries no input name.
    input: Option<String>,
    kind: OverrideItemKind,
    id: String,
    patch: ItemOverridePatch,
    hint: Option<String>,
}

/// Validates a patch for its item kind through the shared field rules of the override model.
fn validate_patch(patch: &ItemOverridePatch, kind: OverrideItemKind, id: &str) -> Vec<FieldError> {
    let mut probe = ItemOverride::default();
    probe.apply_patch(patch);
    probe.validate(kind).into_iter().map(|error| error.with_entry(id)).collect()
}

fn normalize_patches(
    scope: &OverrideScope,
    aliases: &InputAliasMap,
    patches: Vec<ItemPatchDto>,
) -> Result<Vec<NormalizedPatch>, OverrideApiError> {
    let mut normalized = Vec::with_capacity(patches.len());
    let mut seen: HashSet<(Option<String>, OverrideItemKind, String)> = HashSet::new();
    for patch in patches {
        let id = patch.id.trim().to_string();
        if id.is_empty() {
            return Err(OverrideApiError::bad_request("Every patch needs a non-empty item id"));
        }
        let input = match scope {
            OverrideScope::Input(scope_input) => {
                if let Some(requested) = patch.input.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                    let parent = parent_input_name(aliases, requested);
                    if &parent != scope_input {
                        return Err(OverrideApiError::bad_request(format!(
                            "Input override file '{scope_input}' cannot hold a patch for input '{requested}'; \
                             split the save per input"
                        )));
                    }
                }
                None
            }
            OverrideScope::Target(_) => {
                let Some(requested) = patch.input.as_deref().map(str::trim).filter(|name| !name.is_empty()) else {
                    return Err(OverrideApiError::bad_request("A target override patch must name its input"));
                };
                Some(parent_input_name(aliases, requested))
            }
        };
        if !seen.insert((input.clone(), patch.kind, id.clone())) {
            return Err(OverrideApiError::bad_request(format!(
                "Duplicate patch for {} item '{id}'",
                kind_name(patch.kind)
            )));
        }
        let field_errors = validate_patch(&patch.patch, patch.kind, &id);
        if !field_errors.is_empty() {
            return Err(OverrideApiError::FieldErrors(field_errors));
        }
        normalized.push(NormalizedPatch { input, kind: patch.kind, id, patch: patch.patch, hint: patch.hint });
    }
    Ok(normalized)
}

/// The requested hint keys per parent input.
fn wanted_hints(scope: &OverrideScope, patches: &[NormalizedPatch]) -> BTreeMap<String, WantedHints> {
    let mut wanted: BTreeMap<String, WantedHints> = BTreeMap::new();
    for patch in patches {
        let parent = patch.input.clone().unwrap_or_else(|| scope.name().to_string());
        wanted.entry(parent).or_default().insert((patch.kind, patch.id.clone()));
    }
    wanted
}

async fn build_patch_items(
    app_state: &Arc<AppState>,
    aliases: &InputAliasMap,
    scope: &OverrideScope,
    patches: Vec<NormalizedPatch>,
) -> Vec<OverridePatchItem> {
    let sources = app_state.app_config.sources.load();
    let mut hint_inputs: Vec<(String, WantedHints, Arc<ConfigInput>)> = Vec::new();
    for (parent, wanted) in wanted_hints(scope, &patches) {
        // A parent that is no longer configured cannot contribute a hint.
        if let Some(input) = sources.inputs.iter().find(|input| input.name.as_ref() == parent).cloned() {
            hint_inputs.push((parent, wanted, input));
        }
    }
    drop(sources);
    let mut indexes: HashMap<String, HintIndex> = HashMap::new();
    for (parent, wanted, input) in hint_inputs {
        indexes.insert(parent, hint_index(app_state, &input, aliases, &wanted).await);
    }

    patches
        .into_iter()
        .map(|patch| {
            let parent = patch.input.clone().unwrap_or_else(|| scope.name().to_string());
            let hint = indexes
                .get(&parent)
                .and_then(|index| index.get(&(patch.kind, patch.id.clone())).cloned())
                .or_else(|| patch.hint.clone())
                .unwrap_or_default();
            OverridePatchItem { kind: patch.kind, input: patch.input, id: patch.id, patch: patch.patch, hint }
        })
        .collect()
}

async fn current_revision(app_state: &Arc<AppState>, scope: &OverrideScope) -> String {
    match app_state.override_repository().load(scope).await {
        Ok(loaded) => loaded.revision.to_string(),
        Err(err) => {
            warn!("Failed to read the revision of override file '{}': {err}", scope.name());
            OverrideRevision::empty().to_string()
        }
    }
}

async fn write_error(app_state: &Arc<AppState>, scope: &OverrideScope, err: OverrideWriteError) -> OverrideApiError {
    match err {
        OverrideWriteError::Conflict { current } => OverrideApiError::Conflict {
            current_revision: current.to_string(),
            message: Some("The override file changed since it was loaded; reload and repeat the change".to_string()),
        },
        OverrideWriteError::Malformed { error: message } => OverrideApiError::Conflict {
            current_revision: current_revision(app_state, scope).await,
            message: Some(format!("The override file cannot be patched because it is malformed: {message}")),
        },
        OverrideWriteError::UnsupportedSchema { version } => OverrideApiError::Conflict {
            current_revision: current_revision(app_state, scope).await,
            message: Some(format!(
                "The override file cannot be patched because its schema version {version} is not supported"
            )),
        },
        OverrideWriteError::InvalidPatch(message) => OverrideApiError::bad_request(message),
        OverrideWriteError::InvalidName(error) => OverrideApiError::bad_request(error.to_string()),
        OverrideWriteError::Io(error) => {
            OverrideApiError::internal(format!("Failed to write the override file of '{}': {error}", scope.name()))
        }
        OverrideWriteError::Serialization(error) => {
            OverrideApiError::internal(format!("Failed to serialize the override file of '{}': {error}", scope.name()))
        }
    }
}

async fn save_overrides(
    app_state: &Arc<AppState>,
    request: OverrideSaveRequest,
) -> Result<OverrideRevisionDto, OverrideApiError> {
    if request.patches.is_empty() {
        return Err(OverrideApiError::bad_request("The patch list is empty; nothing to save"));
    }
    let resolved = resolve_scope(app_state, request.scope, &request.name)?;
    let scope = resolved.scope;
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);
    let patches = normalize_patches(&scope, &aliases, request.patches)?;
    let patch_items = build_patch_items(app_state, &aliases, &scope, patches).await;
    let base = OverrideRevision(request.base_revision);
    match app_state.override_repository().apply_patches(&scope, &base, patch_items).await {
        Ok(revision) => Ok(OverrideRevisionDto { revision: revision.to_string() }),
        Err(err) => Err(write_error(app_state, &scope, err).await),
    }
}

pub(super) async fn playlist_overrides_save(
    State(app_state): State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<OverrideSaveRequest>,
) -> Response {
    match save_overrides(&app_state, request).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// GET /playlist/overrides/item
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct ItemProvenanceQuery {
    target_id: Option<u16>,
    input: Option<String>,
    kind: Option<String>,
    id: Option<String>,
}

/// Targets that consume `parent` and therefore share the fate of its override file.
///
/// Source inputs are matched by their parent input, so a source that names an alias still counts as a
/// consumer; a target referenced by more than one source block is reported once, in configuration
/// order.
fn targets_for_input(
    app_config: &crate::model::AppConfig,
    aliases: &InputAliasMap,
    parent: &str,
    only: Option<&ConfigTarget>,
) -> Vec<String> {
    if let Some(target) = only {
        return vec![target.name.clone()];
    }
    let sources = app_config.sources.load();
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for source in &sources.sources {
        if !source.inputs.iter().any(|input| parent_input_name(aliases, input) == parent) {
            continue;
        }
        for target in &source.targets {
            if seen.insert(target.name.clone()) {
                names.push(target.name.clone());
            }
        }
    }
    names
}

async fn item_provenance(
    app_state: &Arc<AppState>,
    query: &ItemProvenanceQuery,
) -> Result<OverrideProvenanceDto, OverrideApiError> {
    let input_name = query
        .input
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OverrideApiError::bad_request("The 'input' parameter is required"))?
        .to_string();
    let kind = parse_kind(query.kind.as_deref())?;
    let id = query
        .id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OverrideApiError::bad_request("The 'id' parameter is required"))?
        .to_string();

    let resolved_input = resolve_scope(app_state, OverrideScopeDto::Input, &input_name)?;
    let parent = resolved_input.scope.name().to_string();
    let Some(input) = resolved_input.input else {
        return Err(OverrideApiError::not_found(format!("Unknown input '{input_name}'")));
    };
    let target = match query.target_id {
        Some(target_id) => Some(
            app_state
                .app_config
                .get_target_by_id(target_id)
                .ok_or_else(|| OverrideApiError::not_found(format!("Unknown target id {target_id}")))?,
        ),
        None => None,
    };

    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);

    let key = OverrideKey::new(kind, Arc::<str>::from(id.as_str()));
    let matches = scan_input_item_matches(app_state, &input, &aliases, &key).await?;
    let provider = matches.first().cloned();
    let ambiguous = matches.len() > 1;

    let input_scope = OverrideScope::Input(parent.clone());
    let input_file = load_scope(app_state, &input_scope).await?;
    let input_override = match &input_file.parsed {
        OverrideParseResult::Valid { file, .. } => stored_item(file, None, kind, &id).map(fields_of),
        _ => None,
    };
    let input_revision = input_file.revision.clone();

    let mut target_override = None;
    let mut processed = None;
    // The target file read below is reused for its pending check instead of being read a second time.
    let mut loaded_target = None;
    if let Some(target) = &target {
        let target_scope = OverrideScope::Target(target.name.clone());
        let target_file = load_scope(app_state, &target_scope).await?;
        if let OverrideParseResult::Valid { file, .. } = &target_file.parsed {
            target_override = stored_item(file, Some(&parent), kind, &id).map(fields_of);
        }
        processed = scan_target_item(app_state, target, &aliases, kind, &id).await?;
        loaded_target = Some((target.name.clone(), target_file.revision));
    }

    let status = app_state.override_status_store();
    let repository = app_state.override_repository();
    let mut pending_per_target = Vec::new();
    for target_name in targets_for_input(&app_state.app_config, &aliases, &parent, target.as_deref()) {
        let target_scope = OverrideScope::Target(target_name.clone());
        let target_revision = match &loaded_target {
            Some((name, revision)) if name == &target_name => revision.clone(),
            _ => repository
                .load(&target_scope)
                .await
                .map_or_else(|_| OverrideRevision::empty(), |loaded| loaded.revision),
        };
        let input_pending = status.is_pending(&target_name, &input_scope, &input_revision).await;
        let target_pending = status.is_pending(&target_name, &target_scope, &target_revision).await;
        pending_per_target.push((target_name, input_pending || target_pending));
    }

    Ok(OverrideProvenanceDto { provider, input_override, target_override, processed, pending_per_target, ambiguous })
}

pub(super) async fn playlist_override_item(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<ItemProvenanceQuery>,
) -> Response {
    match item_provenance(&app_state, &query).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// GET /playlist/overrides/orphans
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct OrphansQuery {
    input: Option<String>,
}

/// Whether the input's last run loaded successfully; a failed or missing run never marks entries
/// as orphaned.
async fn input_is_loaded_successfully(app_state: &Arc<AppState>, input_name: &str) -> bool {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let storage_path = crate::processing::input_cache::resolve_input_storage_path(&storage_dir, input_name).await;
    let status = crate::processing::input_cache::load_input_status_async(&storage_path).await;
    !status.clusters.is_empty()
        && status.clusters.values().all(|cluster| cluster.status == crate::processing::input_cache::ClusterState::Ok)
}

fn orphan_state(
    keys: Option<&HashSet<(OverrideItemKind, &str)>>,
    kind: OverrideItemKind,
    id: &str,
) -> OverrideEntryState {
    match keys {
        None => OverrideEntryState::Unknown,
        Some(keys) if keys.contains(&(kind, id)) => OverrideEntryState::Active,
        Some(_) => OverrideEntryState::Orphaned,
    }
}

fn push_orphan_entries(
    entries: &mut Vec<OverrideOrphanEntryDto>,
    scope: OverrideScopeDto,
    file: &str,
    input: Option<&str>,
    sections: &InputOverrideSections,
    keys: Option<&HashSet<(OverrideItemKind, &str)>>,
) {
    for kind in [OverrideItemKind::Live, OverrideItemKind::Vod, OverrideItemKind::Series] {
        for (id, item) in section(sections, kind) {
            entries.push(OverrideOrphanEntryDto {
                scope,
                file: file.to_string(),
                input: input.map(ToString::to_string),
                kind,
                id: id.clone(),
                hint: item.hint.clone(),
                state: orphan_state(keys, kind, id),
            });
        }
    }
}

/// Names of the configured inputs (including the parents aliases point to) and targets.
fn configured_names(app_state: &Arc<AppState>) -> (HashSet<String>, HashSet<String>) {
    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    let inputs = sources
        .inputs
        .iter()
        .flat_map(|input| {
            let runtime = input.name.to_string();
            let parent = parent_input_name(&aliases, &runtime);
            [runtime, parent]
        })
        .collect();
    let targets =
        sources.sources.iter().flat_map(|source| source.targets.iter().map(|target| target.name.clone())).collect();
    (inputs, targets)
}

/// Override files whose name matches no configured input or target, e.g. after a rename.
async fn orphaned_files(app_state: &Arc<AppState>) -> Vec<OrphanedOverrideFileDto> {
    let (known_inputs, known_targets) = configured_names(app_state);
    let repository = app_state.override_repository();
    let mut orphaned = Vec::new();
    for (scope, path) in repository.list_files().await {
        let name = scope.name().to_string();
        let known = match scope {
            OverrideScope::Input(_) => known_inputs.contains(&name),
            OverrideScope::Target(_) => known_targets.contains(&name),
        };
        if known {
            continue;
        }
        let revision = match tokio::fs::read(&path).await {
            Ok(bytes) => OverrideRevision::from_bytes(&bytes).to_string(),
            Err(err) => {
                warn!("Failed to read orphaned override file {}: {err}", path.display());
                OverrideRevision::empty().to_string()
            }
        };
        orphaned.push(OrphanedOverrideFileDto { scope: scope_dto_of(&scope), name, revision });
    }
    orphaned
}

async fn orphan_entries(
    app_state: &Arc<AppState>,
    query: &OrphansQuery,
) -> Result<OverrideOrphansDto, OverrideApiError> {
    let input_name = query
        .input
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OverrideApiError::bad_request("The 'input' parameter is required"))?
        .to_string();
    let resolved = resolve_scope(app_state, OverrideScopeDto::Input, &input_name)?;
    let parent = resolved.scope.name().to_string();
    let Some(input) = resolved.input else {
        return Err(OverrideApiError::not_found(format!("Unknown input '{input_name}'")));
    };

    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);

    let scan = scan_input_inventory(app_state, &input, &aliases).await?;
    let trusted = scan.is_authoritative() && input_is_loaded_successfully(app_state, &parent).await;
    let keys = trusted.then(|| scan.keys());

    let mut entries = Vec::new();
    let input_scope = OverrideScope::Input(parent.clone());
    let input_file = load_scope(app_state, &input_scope).await?;
    if let OverrideParseResult::Valid { file: ParsedOverrideFile::Input(file), .. } = &input_file.parsed {
        push_orphan_entries(&mut entries, OverrideScopeDto::Input, &parent, None, &file.sections, keys.as_ref());
    }

    let repository = app_state.override_repository();
    for (scope, _path) in repository.list_files().await {
        let OverrideScope::Target(target_name) = scope else {
            continue;
        };
        let target_scope = OverrideScope::Target(target_name.clone());
        let Ok(loaded) = repository.load(&target_scope).await else {
            continue;
        };
        if let OverrideParseResult::Valid { file: ParsedOverrideFile::Target(file), .. } = &loaded.parsed {
            if let Some(sections) = file.inputs.get(&parent) {
                push_orphan_entries(
                    &mut entries,
                    OverrideScopeDto::Target,
                    &target_name,
                    Some(&parent),
                    sections,
                    keys.as_ref(),
                );
            }
        }
    }

    let orphaned_files = orphaned_files(app_state).await;
    Ok(OverrideOrphansDto { input: parent, entries, orphaned_files })
}

pub(super) async fn playlist_overrides_orphans(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<OrphansQuery>,
) -> Response {
    match orphan_entries(&app_state, &query).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /playlist/overrides/discard
// ---------------------------------------------------------------------------------------------

/// A patch resetting every field, so the entry disappears once nothing is left to keep.
fn discard_patch(existing: Option<&ItemOverride>) -> ItemOverridePatch {
    ItemOverridePatch {
        audio_track: Some(OverrideFieldOp::Reset),
        cast: Some(OverrideFieldOp::Reset),
        chno: Some(OverrideFieldOp::Reset),
        director: Some(OverrideFieldOp::Reset),
        epg_channel_id: Some(OverrideFieldOp::Reset),
        genre: Some(OverrideFieldOp::Reset),
        group: Some(OverrideFieldOp::Reset),
        logo: Some(OverrideFieldOp::Reset),
        logo_small: Some(OverrideFieldOp::Reset),
        name: Some(OverrideFieldOp::Reset),
        parent_code: Some(OverrideFieldOp::Reset),
        plot: Some(OverrideFieldOp::Reset),
        rating: Some(OverrideFieldOp::Reset),
        rec: Some(OverrideFieldOp::Reset),
        release_date: Some(OverrideFieldOp::Reset),
        time_shift: Some(OverrideFieldOp::Reset),
        title: Some(OverrideFieldOp::Reset),
        tmdb: Some(OverrideFieldOp::Reset),
        remove_unknown: existing.map(|item| item.unknown.keys().cloned().collect()).unwrap_or_default(),
    }
}

/// The scope of a file that exists on disk but whose name is no longer configured.
///
/// An override file outlives the input or target it was written for, so it has to stay discardable
/// after a rename. Only the existence of a file in the repository's own listing grants the scope:
/// an arbitrary name still answers `404`.
async fn existing_file_scope(
    app_state: &Arc<AppState>,
    scope_dto: OverrideScopeDto,
    name: &str,
) -> Option<OverrideScope> {
    let scope = match scope_dto {
        OverrideScopeDto::Input => OverrideScope::Input(name.to_string()),
        OverrideScopeDto::Target => OverrideScope::Target(name.to_string()),
    };
    app_state.override_repository().list_files().await.iter().any(|(existing, _)| existing == &scope).then_some(scope)
}

async fn discard_entries(
    app_state: &Arc<AppState>,
    request: OverrideDiscardRequest,
) -> Result<OverrideRevisionDto, OverrideApiError> {
    let scope = match resolve_scope(app_state, request.scope, &request.name) {
        Ok(resolved) => resolved.scope,
        Err(OverrideApiError::NotFound(message)) => existing_file_scope(app_state, request.scope, &request.name)
            .await
            .ok_or(OverrideApiError::NotFound(message))?,
        Err(err) => return Err(err),
    };

    // An empty item list discards the whole file, which is how an orphaned file is removed.
    if request.items.is_empty() {
        let base = OverrideRevision(request.base_revision);
        return match app_state.override_repository().delete_file(&scope, &base).await {
            Ok(revision) => Ok(OverrideRevisionDto { revision: revision.to_string() }),
            Err(err) => Err(write_error(app_state, &scope, err).await),
        };
    }

    let sources = app_state.app_config.sources.load();
    let aliases = InputAliasMap::from_inputs(&sources.inputs);
    drop(sources);

    let mut seen: HashSet<(Option<String>, OverrideItemKind, String)> = HashSet::new();
    let mut discarded: Vec<(Option<String>, OverrideItemKind, String)> = Vec::new();
    for item in request.items {
        let id = item.id.trim().to_string();
        if id.is_empty() {
            return Err(OverrideApiError::bad_request("Every discard entry needs a non-empty item id"));
        }
        let input = match &scope {
            OverrideScope::Input(scope_input) => {
                if let Some(requested) = item.input.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                    let parent = parent_input_name(&aliases, requested);
                    if &parent != scope_input {
                        return Err(OverrideApiError::bad_request(format!(
                            "Input override file '{scope_input}' cannot hold an entry of input '{requested}'"
                        )));
                    }
                }
                None
            }
            OverrideScope::Target(_) => {
                let Some(requested) = item.input.as_deref().map(str::trim).filter(|name| !name.is_empty()) else {
                    return Err(OverrideApiError::bad_request("A target discard entry must name its input"));
                };
                Some(parent_input_name(&aliases, requested))
            }
        };
        if !seen.insert((input.clone(), item.kind, id.clone())) {
            return Err(OverrideApiError::bad_request(format!(
                "Duplicate discard entry for {} '{id}'",
                kind_name(item.kind)
            )));
        }
        discarded.push((input, item.kind, id));
    }

    let loaded = load_scope(app_state, &scope).await?;
    let parsed = match &loaded.parsed {
        OverrideParseResult::Valid { file, .. } => file,
        OverrideParseResult::Malformed { error } => {
            return Err(OverrideApiError::Conflict {
                current_revision: loaded.revision.to_string(),
                message: Some(format!("The override file is malformed and cannot be patched: {error}")),
            })
        }
        OverrideParseResult::UnsupportedSchema { version } => {
            return Err(OverrideApiError::Conflict {
                current_revision: loaded.revision.to_string(),
                message: Some(format!("The override file has unsupported schema version {version}")),
            })
        }
    };

    let patches = discarded
        .into_iter()
        .map(|(input, kind, id)| {
            let existing = stored_item(parsed, input.as_deref(), kind, &id);
            OverridePatchItem { kind, input, id, patch: discard_patch(existing), hint: String::new() }
        })
        .collect::<Vec<_>>();

    let base = OverrideRevision(request.base_revision);
    match app_state.override_repository().apply_patches(&scope, &base, patches).await {
        Ok(revision) => Ok(OverrideRevisionDto { revision: revision.to_string() }),
        Err(err) => Err(write_error(app_state, &scope, err).await),
    }
}

pub(super) async fn playlist_overrides_discard(
    State(app_state): State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<OverrideDiscardRequest>,
) -> Response {
    match discard_entries(&app_state, request).await {
        Ok(dto) => ok_json(dto),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// GET /playlist/epg/search
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct EpgSearchQuery {
    target_id: Option<u16>,
    input: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
}

/// Case-insensitive matcher that ranks a prefix match above a substring match.
struct SearchNeedle {
    needle: String,
    ascii: bool,
}

impl SearchNeedle {
    fn new(text: &str) -> Self {
        let needle = text.trim().to_lowercase();
        Self { ascii: needle.is_ascii(), needle }
    }

    /// `Some(0)` for a prefix match, `Some(1)` for a substring match.
    fn rank(&self, haystack: &str) -> Option<u8> {
        if self.ascii {
            let haystack = haystack.as_bytes();
            let needle = self.needle.as_bytes();
            if haystack.len() < needle.len() {
                return None;
            }
            if haystack.get(..needle.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle)) {
                return Some(0);
            }
            haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle)).then_some(1)
        } else {
            let haystack = haystack.to_lowercase();
            if haystack.starts_with(&self.needle) {
                Some(0)
            } else {
                haystack.contains(&self.needle).then_some(1)
            }
        }
    }

    fn rank_of_channel(&self, id: &str, display_name: &str) -> Option<u8> {
        match (self.rank(id), self.rank(display_name)) {
            (Some(by_id), Some(by_name)) => Some(by_id.min(by_name)),
            (Some(rank), None) | (None, Some(rank)) => Some(rank),
            (None, None) => None,
        }
    }
}

/// A matched channel: rank, id and display name.
type ChannelMatch = (u8, String, String);

fn collect_match(matches: &mut Vec<ChannelMatch>, needle: &SearchNeedle, id: &str, title: Option<&str>) {
    let display_name = title.map(str::trim).filter(|title| !title.is_empty()).unwrap_or(id);
    if let Some(rank) = needle.rank_of_channel(id, display_name) {
        matches.push((rank, id.to_string(), display_name.to_string()));
    }
}

/// The channels of a target's EPG database, filtered by `text`.
async fn target_epg_matches(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    text: &str,
) -> Result<Vec<ChannelMatch>, OverrideApiError> {
    let config = app_state.app_config.config.load();
    let epg_path = get_epg_path_for_target_by_type(&config, target, TargetType::Xtream)
        .or_else(|| get_epg_path_for_target_by_type(&config, target, TargetType::M3u));
    drop(config);
    let Some(epg_path) = epg_path else {
        // A target that has produced no EPG database yet has nothing to match; it is not an error.
        return Ok(Vec::new());
    };
    // A configured target that has not produced an EPG database yet has nothing to match.
    if !tokio::fs::try_exists(&epg_path).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let needle = SearchNeedle::new(text);
    with_epg_query(&app_state.epg_scans, &app_state.app_config.file_locks, &epg_path, move |query, epg_path, cancel| {
        let mut matches = Vec::new();
        for entry in query.iter() {
            check_scan_cancelled(cancel)?;
            let (_key, channel) = entry.map_err(|err| {
                TuliproxError::RepositoryEpg(format!("Cannot read EPG database {}: {err}", epg_path.display()))
            })?;
            collect_match(&mut matches, &needle, &channel.id, channel.title.as_deref());
            // The rest of the database can only produce matches the answer no longer shows.
            if matches.len() >= EPG_SEARCH_MAX_MATCHES {
                break;
            }
        }
        Ok(matches)
    })
    .await
    .map_err(|err| OverrideApiError::internal(format!("Failed to search the EPG database: {err}")))
}

/// The channels of an input's stored EPG cache, filtered by `text`.
///
/// Only data already on disk is searched: a missing or unreadable cache file yields no candidates
/// for that source and never triggers a provider request.
async fn input_epg_matches(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    text: &str,
) -> Result<Vec<ChannelMatch>, OverrideApiError> {
    let channels: Vec<EpgChannel> = load_cached_epg_channels_for_input(app_state, input).await;
    let needle = SearchNeedle::new(text);
    let mut matches = Vec::new();
    for channel in &channels {
        collect_match(&mut matches, &needle, &channel.id, channel.title.as_deref());
        if matches.len() >= EPG_SEARCH_MAX_MATCHES {
            break;
        }
    }
    Ok(matches)
}

async fn epg_search(
    app_state: &Arc<AppState>,
    query: &EpgSearchQuery,
) -> Result<Vec<EpgSearchResultDto>, OverrideApiError> {
    let text = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| OverrideApiError::bad_request("The 'q' parameter is required"))?
        .to_string();
    let limit = match query.limit.filter(|limit| *limit > 0) {
        Some(limit) => limit.min(EPG_SEARCH_MAX_LIMIT),
        None => EPG_SEARCH_DEFAULT_LIMIT,
    } as usize;

    let (target_id, input_name) = match (query.target_id, non_empty(query.input.clone())) {
        (Some(_), Some(_)) => {
            return Err(OverrideApiError::bad_request("Give either 'target_id' or 'input', not both"))
        }
        (None, None) => return Err(OverrideApiError::bad_request("Exactly one of 'target_id' or 'input' is required")),
        (Some(target_id), None) => (Some(target_id), None),
        (None, Some(input)) => (None, Some(input)),
    };

    let mut matches = if let Some(target_id) = target_id {
        let target = app_state
            .app_config
            .get_target_by_id(target_id)
            .ok_or_else(|| OverrideApiError::not_found(format!("Unknown target id {target_id}")))?;
        target_epg_matches(app_state, &target, &text).await?
    } else if let Some(input_name) = input_name {
        let resolved = resolve_scope(app_state, OverrideScopeDto::Input, &input_name)?;
        let Some(input) = resolved.input else {
            return Err(OverrideApiError::not_found(format!("Unknown input '{input_name}'")));
        };
        input_epg_matches(app_state, &input, &text).await?
    } else {
        Vec::new()
    };

    matches.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    matches.truncate(limit);
    Ok(matches.into_iter().map(|(_, id, display_name)| EpgSearchResultDto { id, display_name }).collect())
}

pub(super) async fn playlist_epg_search(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<EpgSearchQuery>,
) -> Response {
    match epg_search(&app_state, &query).await {
        Ok(results) => ok_json(results),
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests;
