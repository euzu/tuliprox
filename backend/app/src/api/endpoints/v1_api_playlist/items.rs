//! Read-only inspection of the stored playlist databases: `GET /playlist/sources` and `GET /playlist/items`.
//!
//! Only data that is already persisted is read; no provider is contacted. The playback `url` of a record is never
//! returned: provider URLs carry credentials in their path, which no redaction downstream can recognize.

use super::{
    epg_inspect::{load_epg_keys, target_epg_case},
    inspect_common::{
        parse_enum_list, query_enum, CaseInsensitiveNeedle, InspectError, PageWindow, DEFAULT_PAGE_LIMIT,
        MAX_PAGE_LIMIT,
    },
    recording::find_target_by_name,
};
use crate::{
    api::{
        endpoints::{extract_form_query::FormQuery, xmltv_api::get_epg_path_for_target_by_type},
        model::AppState,
    },
    model::{ConfigInput, ConfigTarget},
    repository::{
        iter_raw_m3u_input_playlist, iter_raw_m3u_target_playlist, iter_raw_xtream_input_playlist,
        iter_raw_xtream_target_playlist,
    },
    utils::{with_folded_epg_id, EpgIdOutputCase},
};
use axum::{extract::State, response::IntoResponse};
use log::error;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use shared::{
    error::TuliproxError,
    model::{
        InputType, M3uPlaylistItem, PlaylistItemType, StreamProperties, TargetType, XtreamCluster, XtreamPlaylistItem,
    },
};
use std::{borrow::Cow, collections::HashSet, sync::Arc};
use tokio_stream::{Stream, StreamExt};

query_enum! {
    /// Record fields a client can project with `fields`.
    pub(in crate::api::endpoints) enum ItemField {
        VirtualId => "virtual_id",
        ProviderId => "provider_id",
        Name => "name",
        Title => "title",
        Group => "group",
        ItemType => "item_type",
        Cluster => "cluster",
        EpgChannelId => "epg_channel_id",
        CategoryId => "category_id",
        ChannelNo => "channel_no",
        Logo => "logo",
        LogoSmall => "logo_small",
        ParentCode => "parent_code",
        Rec => "rec",
        InputName => "input_name",
        InputStreamId => "input_stream_id",
        SourceOrdinal => "source_ordinal",
        AdditionalProperties => "additional_properties",
        EpgStatus => "epg_status",
    }
}

query_enum! {
    /// Text fields `search` is matched against.
    pub(in crate::api::endpoints) enum ItemSearchField {
        Name => "name",
        Title => "title",
        Group => "group",
        EpgChannelId => "epg_channel_id",
        ProviderId => "provider_id",
        InputName => "input_name",
    }
}

query_enum! {
    /// How a playlist item's `epg_channel_id` relates to the target's EPG database.
    pub(in crate::api::endpoints) enum EpgStatus {
        MissingId => "missing_id",
        Unmatched => "unmatched",
        Matched => "matched",
    }
}

/// Fields returned when `fields` is not given. `epg_status` is opt-in.
pub(in crate::api::endpoints) const DEFAULT_ITEM_FIELDS: &[ItemField] = &[
    ItemField::VirtualId,
    ItemField::ProviderId,
    ItemField::Name,
    ItemField::Title,
    ItemField::Group,
    ItemField::ItemType,
    ItemField::Cluster,
    ItemField::EpgChannelId,
    ItemField::CategoryId,
    ItemField::ChannelNo,
];

/// Fields `search` covers when `search_field` is not given.
pub(in crate::api::endpoints) const DEFAULT_SEARCH_FIELDS: &[ItemSearchField] =
    &[ItemSearchField::Name, ItemSearchField::Title, ItemSearchField::Group];

/// Playlist cluster as named on the wire.
const fn cluster_name(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => "live",
        XtreamCluster::Video => "video",
        XtreamCluster::Series => "series",
    }
}

fn parse_cluster(value: Option<&str>) -> Result<XtreamCluster, InspectError> {
    match value.map(str::trim) {
        Some("live") => Ok(XtreamCluster::Live),
        Some("video") => Ok(XtreamCluster::Video),
        Some("series") => Ok(XtreamCluster::Series),
        Some("") | None => Err(InspectError::bad_request("Missing cluster; allowed values: live, video, series")),
        Some(other) => {
            let shown: String = other.chars().take(64).collect();
            Err(InspectError::bad_request(format!("Unknown cluster '{shown}'; allowed values: live, video, series")))
        }
    }
}

/// A stored playlist record, normalized across the Xtream and M3U representations.
///
/// Implementors only map their own field layout onto these accessors; filtering and projection are written once
/// against the trait.
pub(super) trait StoredPlaylistRecord {
    fn virtual_id(&self) -> u32;
    fn provider_id(&self) -> Cow<'_, str>;
    fn name(&self) -> &str;
    fn title(&self) -> &str;
    fn group(&self) -> &str;
    fn logo(&self) -> &str;
    fn logo_small(&self) -> &str;
    fn parent_code(&self) -> &str;
    fn rec(&self) -> &str;
    fn input_name(&self) -> &str;
    fn input_stream_id(&self) -> &str;
    fn epg_channel_id(&self) -> Option<&str>;
    fn item_type(&self) -> PlaylistItemType;
    fn cluster(&self) -> XtreamCluster;
    fn category_id(&self) -> Option<u32>;
    fn channel_no(&self) -> u32;
    fn source_ordinal(&self) -> u32;
    fn additional_properties(&self) -> Option<&StreamProperties>;
}

/// Accessors for the `Arc<str>` fields both record types share under the same name.
macro_rules! shared_str_accessors {
    ($($field:ident),+ $(,)?) => {
        $(fn $field(&self) -> &str { &self.$field })+
    };
}

impl StoredPlaylistRecord for XtreamPlaylistItem {
    shared_str_accessors!(name, title, group, logo, logo_small, parent_code, rec, input_name, input_stream_id);

    fn virtual_id(&self) -> u32 { self.virtual_id.get() }

    fn provider_id(&self) -> Cow<'_, str> { Cow::Owned(self.provider_id.to_string()) }

    fn epg_channel_id(&self) -> Option<&str> { self.epg_channel_id.as_deref() }

    fn item_type(&self) -> PlaylistItemType { self.item_type }

    fn cluster(&self) -> XtreamCluster { self.xtream_cluster }

    fn category_id(&self) -> Option<u32> { Some(self.category_id) }

    fn channel_no(&self) -> u32 { self.channel_no }

    fn source_ordinal(&self) -> u32 { self.source_ordinal }

    fn additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
}

impl StoredPlaylistRecord for M3uPlaylistItem {
    shared_str_accessors!(name, title, group, logo, logo_small, parent_code, rec, input_name, input_stream_id);

    fn virtual_id(&self) -> u32 { self.virtual_id.get() }

    fn provider_id(&self) -> Cow<'_, str> { Cow::Borrowed(&self.provider_id) }

    fn epg_channel_id(&self) -> Option<&str> { self.epg_channel_id.as_deref() }

    fn item_type(&self) -> PlaylistItemType { self.item_type }

    fn cluster(&self) -> XtreamCluster { self.item_type.cluster() }

    fn category_id(&self) -> Option<u32> { None }

    fn channel_no(&self) -> u32 { self.chno }

    fn source_ordinal(&self) -> u32 { self.source_ordinal }

    fn additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
}

/// Members of `additional_properties` that are never returned, wherever they occur.
///
/// `direct_source` and the catchup `source`/`extra_attributes` hold provider URLs (credentials live in their path);
/// `video`/`audio` are stored as compressed blobs that mean nothing to a client.
const DROPPED_PROPERTY_KEYS: &[&str] = &["direct_source", "source", "extra_attributes", "video", "audio"];

/// Text that carries credentials in a form that can be recognized by looking at the value alone.
fn looks_credential_bearing(text: &str) -> bool {
    if let Some(scheme_end) = text.find("://") {
        let authority = text[scheme_end + 3..].split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return true;
        }
    }
    if !text.contains('=') {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    ["username=", "password=", "token=", "apikey=", "api_key="].iter().any(|marker| lower.contains(marker))
}

fn is_dropped_property_key(key: &str) -> bool {
    DROPPED_PROPERTY_KEYS.contains(&key) || key == "url" || key.ends_with("_url")
}

fn sanitize_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, member| {
                if is_dropped_property_key(key) {
                    return false;
                }
                sanitize_json(member);
                !matches!(member, Value::String(text) if looks_credential_bearing(text))
            });
        }
        Value::Array(members) => {
            for member in members.iter_mut() {
                sanitize_json(member);
            }
            members.retain(|member| !matches!(member, Value::String(text) if looks_credential_bearing(text)));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// `additional_properties` as JSON with every URL-like or credential-bearing member removed.
pub(in crate::api::endpoints) fn sanitized_additional_properties(properties: &StreamProperties) -> Value {
    let mut value = serde_json::to_value(properties).unwrap_or(Value::Null);
    sanitize_json(&mut value);
    value
}

/// A text value as returned to the client; `null` when it looks credential-bearing.
fn visible_text(text: &str) -> Value {
    if looks_credential_bearing(text) {
        Value::Null
    } else {
        Value::from(text)
    }
}

/// The EPG channel ids of a target's EPG database, for classifying playlist items.
pub(super) struct EpgLookup {
    keys: HashSet<Arc<str>>,
    case: EpgIdOutputCase,
}

impl EpgLookup {
    pub(super) fn new(keys: HashSet<Arc<str>>, case: EpgIdOutputCase) -> Self { Self { keys, case } }

    /// The id is canonicalized like the keys of the EPG database are written (the target's output casing).
    pub(super) fn classify(&self, epg_channel_id: Option<&str>) -> EpgStatus {
        let Some(id) = epg_channel_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return EpgStatus::MissingId;
        };
        let known = match self.case {
            EpgIdOutputCase::Preserve => self.keys.contains(id),
            EpgIdOutputCase::LowercaseAscii => with_folded_epg_id(id, |folded| self.keys.contains(folded)),
        };
        if known {
            EpgStatus::Matched
        } else {
            EpgStatus::Unmatched
        }
    }
}

fn field_value<R: StoredPlaylistRecord>(record: &R, field: ItemField, epg: Option<&EpgLookup>) -> Value {
    match field {
        ItemField::VirtualId => Value::from(record.virtual_id()),
        ItemField::ProviderId => Value::from(record.provider_id().into_owned()),
        ItemField::Name => Value::from(record.name()),
        ItemField::Title => Value::from(record.title()),
        ItemField::Group => Value::from(record.group()),
        ItemField::ItemType => Value::from(record.item_type().as_str()),
        ItemField::Cluster => Value::from(cluster_name(record.cluster())),
        ItemField::EpgChannelId => record.epg_channel_id().map_or(Value::Null, Value::from),
        ItemField::CategoryId => record.category_id().map_or(Value::Null, Value::from),
        ItemField::ChannelNo => Value::from(record.channel_no()),
        ItemField::Logo => visible_text(record.logo()),
        ItemField::LogoSmall => visible_text(record.logo_small()),
        ItemField::ParentCode => Value::from(record.parent_code()),
        ItemField::Rec => Value::from(record.rec()),
        ItemField::InputName => Value::from(record.input_name()),
        ItemField::InputStreamId => Value::from(record.input_stream_id()),
        ItemField::SourceOrdinal => Value::from(record.source_ordinal()),
        ItemField::AdditionalProperties => {
            record.additional_properties().map_or(Value::Null, sanitized_additional_properties)
        }
        ItemField::EpgStatus => {
            epg.map_or(Value::Null, |lookup| Value::from(lookup.classify(record.epg_channel_id()).name()))
        }
    }
}

/// The record as a JSON object holding exactly `fields`, in that order.
fn project<R: StoredPlaylistRecord>(record: &R, fields: &[ItemField], epg: Option<&EpgLookup>) -> Map<String, Value> {
    fields.iter().map(|field| (field.name().to_string(), field_value(record, *field, epg))).collect()
}

fn search_text<R: StoredPlaylistRecord>(record: &R, field: ItemSearchField) -> Cow<'_, str> {
    match field {
        ItemSearchField::Name => Cow::Borrowed(record.name()),
        ItemSearchField::Title => Cow::Borrowed(record.title()),
        ItemSearchField::Group => Cow::Borrowed(record.group()),
        ItemSearchField::EpgChannelId => Cow::Borrowed(record.epg_channel_id().unwrap_or_default()),
        ItemSearchField::ProviderId => record.provider_id(),
        ItemSearchField::InputName => Cow::Borrowed(record.input_name()),
    }
}

/// What a record has to satisfy to be counted; every criterion that is set must hold.
struct ItemFilter<'a> {
    needle: Option<&'a CaseInsensitiveNeedle>,
    search_fields: &'a [ItemSearchField],
    group: Option<&'a str>,
    epg: Option<&'a EpgLookup>,
    epg_status: &'a [EpgStatus],
}

impl ItemFilter<'_> {
    fn matches<R: StoredPlaylistRecord>(&self, record: &R) -> bool {
        if self.group.is_some_and(|group| record.group() != group) {
            return false;
        }
        if let Some(needle) = self.needle {
            if !self.search_fields.iter().any(|field| needle.is_in(&search_text(record, *field))) {
                return false;
            }
        }
        if !self.epg_status.is_empty() {
            let Some(lookup) = self.epg else { return false };
            if !self.epg_status.contains(&lookup.classify(record.epg_channel_id())) {
                return false;
            }
        }
        true
    }
}

/// A target lists a series cluster without the episode rows, like the Web UI playlist does
/// (`get_playlist_for_target`), so counts match what the UI shows.
fn visible_in_target_listing<R: StoredPlaylistRecord>(cluster: XtreamCluster, record: &R) -> bool {
    !cluster.is_series() || !record.item_type().is_series()
}

struct ItemPage {
    total_matched: usize,
    items: Vec<Map<String, Value>>,
}

/// One pass over the stored records: counts every match and keeps only the requested window.
async fn collect_page<R, S>(
    mut records: S,
    visible: impl Fn(&R) -> bool,
    filter: &ItemFilter<'_>,
    fields: &[ItemField],
    window: PageWindow,
) -> Result<ItemPage, TuliproxError>
where
    R: StoredPlaylistRecord,
    S: Stream<Item = Result<R, TuliproxError>> + Unpin,
{
    let mut total_matched = 0_usize;
    let mut items = Vec::with_capacity(window.limit.min(DEFAULT_PAGE_LIMIT as usize));
    while let Some(entry) = records.next().await {
        let record = entry?;
        if !visible(&record) || !filter.matches(&record) {
            continue;
        }
        if window.contains(total_matched) {
            items.push(project(&record, fields, filter.epg));
        }
        total_matched += 1;
    }
    Ok(ItemPage { total_matched, items })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ItemsQuery {
    target: Option<String>,
    input: Option<String>,
    cluster: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    search: Option<String>,
    search_field: Option<Vec<String>>,
    group: Option<String>,
    fields: Option<Vec<String>>,
    epg_status: Option<Vec<String>>,
}

#[derive(Debug)]
enum SourceName {
    Target(String),
    Input(String),
}

/// The request after every parameter has been checked.
#[derive(Debug)]
struct ValidatedItemsQuery {
    source: SourceName,
    cluster: XtreamCluster,
    window: PageWindow,
    needle: Option<CaseInsensitiveNeedle>,
    search_fields: Vec<ItemSearchField>,
    group: Option<String>,
    fields: Vec<ItemField>,
    epg_status: Vec<EpgStatus>,
}

impl ValidatedItemsQuery {
    fn parse(query: ItemsQuery) -> Result<Self, InspectError> {
        let non_empty = |value: Option<String>| value.map(|value| value.trim().to_string()).filter(|v| !v.is_empty());
        let source = match (non_empty(query.target), non_empty(query.input)) {
            (Some(target), None) => SourceName::Target(target),
            (None, Some(input)) => SourceName::Input(input),
            (None, None) => return Err(InspectError::bad_request("Exactly one of 'target' or 'input' is required")),
            (Some(_), Some(_)) => {
                return Err(InspectError::bad_request("Give either 'target' or 'input', not both"));
            }
        };
        let cluster = parse_cluster(query.cluster.as_deref())?;
        let mut search_fields = parse_enum_list::<ItemSearchField>("search_field", query.search_field.as_deref())?;
        if search_fields.is_empty() {
            search_fields.extend_from_slice(DEFAULT_SEARCH_FIELDS);
        }
        let mut fields = parse_enum_list::<ItemField>("fields", query.fields.as_deref())?;
        if fields.is_empty() {
            fields.extend_from_slice(DEFAULT_ITEM_FIELDS);
        }
        let epg_status = parse_enum_list::<EpgStatus>("epg_status", query.epg_status.as_deref())?;
        if matches!(source, SourceName::Input(_)) && (!epg_status.is_empty() || fields.contains(&ItemField::EpgStatus))
        {
            return Err(InspectError::bad_request(
                "epg_status is only available for a target; an input has no EPG database",
            ));
        }
        Ok(Self {
            source,
            cluster,
            window: PageWindow::resolve(query.offset, query.limit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT),
            needle: CaseInsensitiveNeedle::new(query.search.as_deref()),
            search_fields,
            group: query.group.filter(|group| !group.is_empty()),
            fields,
            epg_status,
        })
    }

    fn wants_epg(&self) -> bool { !self.epg_status.is_empty() || self.fields.contains(&ItemField::EpgStatus) }
}

#[derive(Serialize)]
struct SourceRef<'a> {
    kind: &'static str,
    name: &'a str,
}

#[derive(Serialize)]
struct ItemsResponse<'a> {
    source: SourceRef<'a>,
    cluster: &'static str,
    total_matched: usize,
    offset: usize,
    limit: usize,
    limit_clamped: bool,
    returned: usize,
    has_more: bool,
    items: Vec<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

const NO_STORED_PLAYLIST_NOTE: &str =
    "No stored playlist database exists yet for this source (it was never updated); total_matched 0 does not mean the \
     source is empty.";

/// Outcome of reading one source: the records' page, or `None` when no database is stored.
type SourcePage = Option<ItemPage>;

impl<'a> ItemsResponse<'a> {
    fn new(kind: &'static str, name: &'a str, query: &ValidatedItemsQuery, page: SourcePage) -> Self {
        let note = page.is_none().then_some(NO_STORED_PLAYLIST_NOTE);
        let page = page.unwrap_or(ItemPage { total_matched: 0, items: Vec::new() });
        let returned = page.items.len();
        Self {
            source: SourceRef { kind, name },
            cluster: cluster_name(query.cluster),
            total_matched: page.total_matched,
            offset: query.window.offset,
            limit: query.window.limit,
            limit_clamped: query.window.limit_clamped,
            returned,
            has_more: query.window.has_more(page.total_matched, returned),
            items: page.items,
            note,
        }
    }
}

fn read_failure(source: &str, err: &TuliproxError) -> InspectError {
    error!("Failed to read the stored playlist of {source}: {err}");
    InspectError::internal("Failed to read the stored playlist database; see the server log")
}

async fn read_target_page(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    query: &ValidatedItemsQuery,
) -> Result<SourcePage, InspectError> {
    let output = if target.has_output(TargetType::Xtream) {
        TargetType::Xtream
    } else if target.has_output(TargetType::M3u) {
        TargetType::M3u
    } else {
        return Err(InspectError::bad_request(format!(
            "Target '{}' has no xtream or m3u output; its playlist is not stored",
            target.name
        )));
    };

    let lookup = if query.wants_epg() { Some(load_epg_lookup(app_state, target, output).await?) } else { None };
    let filter = ItemFilter {
        needle: query.needle.as_ref(),
        search_fields: &query.search_fields,
        group: query.group.as_deref(),
        epg: lookup.as_ref(),
        epg_status: &query.epg_status,
    };
    let cluster = query.cluster;
    let app_config = &app_state.app_config;
    let page = match output {
        TargetType::Xtream => match iter_raw_xtream_target_playlist(app_config, target, cluster).await {
            Some(records) => {
                collect_page(
                    records,
                    |record: &XtreamPlaylistItem| visible_in_target_listing(cluster, record),
                    &filter,
                    &query.fields,
                    query.window,
                )
                .await
            }
            None => return Ok(None),
        },
        _ => match iter_raw_m3u_target_playlist(app_config, target, Some(cluster)).await {
            Some(records) => {
                collect_page(
                    records,
                    |record: &M3uPlaylistItem| visible_in_target_listing(cluster, record),
                    &filter,
                    &query.fields,
                    query.window,
                )
                .await
            }
            None => return Ok(None),
        },
    };
    page.map(Some).map_err(|err| read_failure(&format!("target '{}'", target.name), &err))
}

async fn read_input_page(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    query: &ValidatedItemsQuery,
) -> Result<SourcePage, InspectError> {
    let filter = ItemFilter {
        needle: query.needle.as_ref(),
        search_fields: &query.search_fields,
        group: query.group.as_deref(),
        epg: None,
        epg_status: &[],
    };
    let cluster = query.cluster;
    let app_config = &app_state.app_config;
    let page = if input.input_type.is_xtream() {
        match iter_raw_xtream_input_playlist(app_config, input, cluster).await {
            Some(records) => {
                collect_page(records, |_: &XtreamPlaylistItem| true, &filter, &query.fields, query.window).await
            }
            None => return Ok(None),
        }
    } else if input.input_type.is_m3u() {
        match iter_raw_m3u_input_playlist(app_config, input, Some(cluster)).await {
            Some(records) => {
                collect_page(records, |_: &M3uPlaylistItem| true, &filter, &query.fields, query.window).await
            }
            None => return Ok(None),
        }
    } else {
        return Err(InspectError::bad_request(unsupported_input_message(input)));
    };
    page.map(Some).map_err(|err| read_failure(&format!("input '{}'", input.name), &err))
}

fn unsupported_input_message(input: &ConfigInput) -> String {
    format!(
        "Input '{}' has type '{}'; only xtream and m3u inputs have a stored playlist database to inspect",
        input.name, input.input_type
    )
}

/// The EPG channel ids of the target's EPG database for `output`.
async fn load_epg_lookup(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    output: TargetType,
) -> Result<EpgLookup, InspectError> {
    let epg_path = get_epg_path_for_target_by_type(&app_state.app_config.config.load(), target, output);
    let Some(epg_path) = epg_path else {
        return Err(InspectError::bad_request(format!(
            "Target '{}' has no EPG database for its {output} output; epg_status cannot be evaluated",
            target.name
        )));
    };
    let keys = load_epg_keys(&app_state.app_config.file_locks, &epg_path).await.map_err(|err| {
        error!("Failed to read the EPG database of target '{}': {err}", target.name);
        InspectError::internal("Failed to read the EPG database; see the server log")
    })?;
    Ok(EpgLookup::new(keys, target_epg_case(target)))
}

async fn run_items_query(app_state: &Arc<AppState>, query: ItemsQuery) -> Result<serde_json::Value, InspectError> {
    let query = ValidatedItemsQuery::parse(query)?;
    let to_json = |response: ItemsResponse<'_>| {
        serde_json::to_value(response)
            .map_err(|err| InspectError::internal(format!("Failed to encode response: {err}")))
    };
    match &query.source {
        SourceName::Target(name) => {
            let target = find_target_by_name(&app_state.app_config, name)
                .ok_or_else(|| InspectError::not_found(format!("Unknown target '{name}'")))?;
            let page = read_target_page(app_state, &target, &query).await?;
            to_json(ItemsResponse::new("target", name, &query, page))
        }
        SourceName::Input(name) => {
            let input = app_state
                .app_config
                .sources
                .load()
                .inputs
                .iter()
                .find(|input| input.name.as_ref() == name.as_str())
                .cloned()
                .ok_or_else(|| InspectError::not_found(format!("Unknown input '{name}'")))?;
            let page = read_input_page(app_state, &input, &query).await?;
            to_json(ItemsResponse::new("input", name, &query, page))
        }
    }
}

/// `GET /playlist/items`: stored records of one target or input, with paging, filter and field projection.
pub(super) async fn query_playlist_items(
    State(app_state): State<Arc<AppState>>,
    FormQuery(query): FormQuery<ItemsQuery>,
) -> axum::response::Response {
    match run_items_query(&app_state, query).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(err) => err.into_response(),
    }
}

#[derive(Serialize)]
struct EpgAvailability {
    output: TargetType,
    available: bool,
}

#[derive(Serialize)]
struct TargetSummary<'a> {
    id: u16,
    name: &'a str,
    outputs: Vec<TargetType>,
    epg: Vec<EpgAvailability>,
}

#[derive(Serialize)]
struct InputSummary<'a> {
    id: u16,
    name: &'a str,
    input_type: InputType,
    /// Clusters whose stored records can be queried; empty when the input type has no stored playlist database.
    clusters: &'static [&'static str],
}

#[derive(Serialize)]
struct SourcesResponse<'a> {
    targets: Vec<TargetSummary<'a>>,
    inputs: Vec<InputSummary<'a>>,
}

const QUERYABLE_CLUSTERS: &[&str] = &["live", "video", "series"];

fn summarize_input(input: &ConfigInput) -> InputSummary<'_> {
    let queryable = input.input_type.is_xtream() || input.input_type.is_m3u();
    InputSummary {
        id: input.id,
        name: &input.name,
        input_type: input.input_type,
        clusters: if queryable { QUERYABLE_CLUSTERS } else { &[] },
    }
}

fn summarize_target<'a>(config: &crate::model::Config, target: &'a ConfigTarget) -> TargetSummary<'a> {
    let outputs: Vec<TargetType> = target.output.iter().map(TargetType::from).collect();
    let epg = outputs
        .iter()
        .copied()
        .filter(|output| output.supports_epg())
        .map(|output| EpgAvailability {
            output,
            available: get_epg_path_for_target_by_type(config, target, output).is_some(),
        })
        .collect();
    TargetSummary { id: target.id, name: &target.name, outputs, epg }
}

/// `GET /playlist/sources`: the targets and inputs whose stored playlists can be inspected.
pub(super) async fn list_playlist_sources(State(app_state): State<Arc<AppState>>) -> axum::response::Response {
    let sources = app_state.app_config.sources.load();
    let config = app_state.app_config.config.load();
    let response = SourcesResponse {
        targets: sources
            .sources
            .iter()
            .flat_map(|source| &source.targets)
            .map(|target| summarize_target(&config, target))
            .collect(),
        inputs: sources.inputs.iter().map(|input| summarize_input(input)).collect(),
    };
    axum::Json(response).into_response()
}

#[cfg(test)]
pub(in crate::api::endpoints::v1_api_playlist) mod tests;
