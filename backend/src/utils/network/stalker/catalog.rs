use log::{info, warn};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use shared::model::stalker::StalkerStreamKind;
use shared::utils::deserialize_number_from_string;

use crate::utils::network::stalker::client::StalkerApiClient;
use crate::utils::network::stalker::error::{StalkerError, StalkerResult};
use crate::utils::network::stalker::profile::StalkerHandshake;
use crate::utils::network::stalker::recipes::recipe_spec_for;
use crate::utils::network::stalker::url_factory::StalkerLoadUrl;

/// A category returned by `get_*_categories`. Stalker portals wrap the list in `{"js": [...]}`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct StalkerCategory {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub number: Option<i32>,
}

impl StalkerCategory {
    fn from_js_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(Self {
            id: obj.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            title: obj.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            alias: obj.get("alias").and_then(Value::as_str).map(String::from),
            number: Some(
                obj.get("number")
                    .and_then(Value::as_i64)
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(0),
            ),
        })
    }
}

/// A live/VOD stream row as the portal returns it. The field set is the union of what
/// every portal flavour we care about emits; unknown fields are silently ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawItem {
    pub id: Option<String>,
    pub number: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub category_id: Option<String>,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub stream_icon: Option<String>,
    #[serde(default)]
    pub epg_channel_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub tv_archive: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_i32ish_option")]
    pub tv_archive_duration: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub allow_local_timeshift: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub allow_pvr: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub pvr: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub pvr_shift: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub pvr_time_shift: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub nginx_secure_link: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub flussonic_tmp_link: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub wowza_tmp_link: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_boolish_option")]
    pub use_http_tmp_link: Option<bool>,
    #[serde(default)]
    pub container_extension: Option<String>,
    #[serde(default)]
    pub info: Option<StalkerRawItemInfo>,
    #[serde(default)]
    pub series_id: Option<String>,
}

impl StalkerRawItem {
    pub fn stream_id(&self) -> Option<u32> { self.id.as_ref().and_then(|s| s.parse::<u32>().ok()) }
    pub fn stream_kind(&self) -> StalkerStreamKind {
        if self.series_id.is_some() { StalkerStreamKind::Episode } else { StalkerStreamKind::Live }
    }
    pub fn display_name(&self) -> &str { self.name.as_deref().or(self.title.as_deref()).unwrap_or("") }
}

fn deserialize_boolish_option<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match value {
        Value::Bool(v) => Some(v),
        Value::Number(n) => n.as_i64().map(|v| v != 0),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else if trimmed.eq_ignore_ascii_case("true") {
                Some(true)
            } else if trimmed.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                trimmed.parse::<i64>().ok().map(|v| v != 0)
            }
        }
        _ => None,
    })
}

fn deserialize_i32ish_option<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_number_from_string(deserializer)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawItemInfo {
    #[serde(default)]
    pub movie_image: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub cast: Option<String>,
    #[serde(default)]
    pub director: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub releasedate: Option<String>,
    #[serde(default)]
    pub rating: Option<f32>,
    #[serde(default)]
    pub tmdb_id: Option<String>,
    #[serde(default)]
    pub backdrop: Option<Vec<String>>,
    #[serde(default)]
    pub age: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
}

/// A series metadata row as returned by `get_series_list`. The portal sometimes returns
/// `id` as a string, sometimes as an integer; we accept both and always store the string
/// form so the rest of the code does not need to care.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawSeriesItem {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub cast: Option<String>,
    #[serde(default)]
    pub director: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub releasedate: Option<String>,
    #[serde(default)]
    pub rating: Option<f32>,
    #[serde(default)]
    pub tmdb_id: Option<String>,
    #[serde(default)]
    pub backdrop: Option<Vec<String>>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub series_id: Option<String>,
    #[serde(default)]
    pub count: Option<u32>,
}

impl StalkerRawSeriesItem {
    pub fn display_name(&self) -> &str { self.name.as_deref().or(self.title.as_deref()).unwrap_or("") }
    pub fn id_string(&self) -> Option<String> {
        self.id.clone().or_else(|| self.series_id.clone())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawSeriesDetails {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub cast: Option<String>,
    #[serde(default)]
    pub director: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub releasedate: Option<String>,
    #[serde(default)]
    pub rating: Option<f32>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub backdrop: Option<Vec<String>>,
    #[serde(default)]
    pub seasons: Vec<StalkerRawSeriesSeason>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawSeriesSeason {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub number: Option<i32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub episodes: Vec<StalkerRawSeriesEpisode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StalkerRawSeriesEpisode {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub number: Option<i32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub season_number: Option<i32>,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub info: Option<StalkerRawItemInfo>,
    #[serde(default)]
    pub container_extension: Option<String>,
}

impl StalkerRawSeriesEpisode {
    pub fn display_name(&self) -> &str { self.name.as_deref().or(self.title.as_deref()).unwrap_or("") }
}

// ----- Generic fetchers -------------------------------------------------------------------------

async fn get_categories(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    portal_type: &'static str,
    action: &'static str,
) -> StalkerResult<Vec<StalkerCategory>> {
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let mut builder = client
            .http()
            .get(&load_url.load_url)
            .headers(client.common_headers(&load_url))
            .query(&[
                ("type", portal_type),
                ("action", action),
                ("JsHttpRequest", "1-xml"),
                ("HttpRequest", "1-xml"),
            ]);
        builder = client.apply_mac_query(builder);
        builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
        match client.send_json::<Value>(builder, action).await {
            Ok(value) => {
                return Ok(parse_categories(&value));
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: client.portal_url().to_string() }))
}

fn parse_categories(value: &Value) -> Vec<StalkerCategory> {
    let mut out = Vec::new();
    match value {
        Value::Object(map) => {
            if let Some(js) = map.get("js") {
                collect_categories(js, &mut out);
            }
            for (k, v) in map {
                if k == "js" {
                    continue;
                }
                if let Some(cat) = StalkerCategory::from_js_value(v) {
                    out.push(cat);
                }
            }
        }
        Value::Array(arr) => collect_categories(&Value::Array(arr.clone()), &mut out),
        _ => {}
    }
    out
}

fn collect_categories(value: &Value, out: &mut Vec<StalkerCategory>) {
    match value {
        Value::Array(arr) => {
            for item in arr {
                if let Some(cat) = StalkerCategory::from_js_value(item) {
                    out.push(cat);
                }
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                if let Some(cat) = StalkerCategory::from_js_value(v) {
                    out.push(cat);
                }
            }
        }
        _ => {}
    }
}

async fn get_paginated_items<T>(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    portal_type: &'static str,
    action: &'static str,
    parse_page: impl Fn(&Value) -> (Vec<T>, Option<u32>, Option<u32>),
) -> StalkerResult<Vec<T>>
where
    T: for<'de> Deserialize<'de> + Clone,
{
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let mut page: u32 = 1;
    let mut all: Vec<T> = Vec::new();
    let page_limit = client.catalog_max_pages();
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let mut pages_fetched: u32 = 0;
        while page <= page_limit {
            let current_page = page;
            let mut builder = client
                .http()
                .get(&load_url.load_url)
                .headers(client.common_headers(&load_url))
                .query(&[
                    ("type", portal_type),
                    ("action", action),
                    ("JsHttpRequest", "1-xml"),
                    ("HttpRequest", "1-xml"),
                    ("p", current_page.to_string().as_str()),
                ]);
            builder = client.apply_mac_query(builder);
            builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
            let value: Value = match client.send_json::<Value>(builder, action).await {
                Ok(v) => v,
                Err(err) => {
                    last_err = Some(err);
                    break;
                }
            };
            let (mut items, total_items, max_page_items) = parse_page(&value);
            let page_len = items.len();
            if items.is_empty() {
                break;
            }
            pages_fetched = pages_fetched.saturating_add(1);
            all.append(&mut items);
            if total_items.is_some_and(|total| all.len() >= usize::try_from(total).unwrap_or(usize::MAX)) {
                info!(
                    "Stalker {portal_type}/{action} fetched {}/{} items across {} pages",
                    all.len(),
                    total_items.unwrap_or_default(),
                    pages_fetched
                );
                return Ok(all);
            }
            if max_page_items.is_some_and(|max_items| page_len < usize::try_from(max_items).unwrap_or(usize::MAX)) {
                info!(
                    "Stalker {portal_type}/{action} fetched {}{} items across {} pages",
                    all.len(),
                    total_items
                        .map(|total| format!("/{total}"))
                        .unwrap_or_default(),
                    pages_fetched
                );
                return Ok(all);
            }
            page += 1;
        }
        if !all.is_empty() {
            if page > page_limit {
                warn!(
                    "Stalker {portal_type}/{action} aborted after reaching page limit {} with {} items fetched",
                    page_limit,
                    all.len()
                );
            }
            info!(
                "Stalker {portal_type}/{action} fetched {} items across {} pages",
                all.len(),
                pages_fetched
            );
            return Ok(all);
        }
        page = 1;
    }
    if all.is_empty() {
        return Err(last_err.unwrap_or_else(|| StalkerError::EmptyBody { action: action.to_string() }));
    }
    Ok(all)
}

fn extract_items_array(value: &Value) -> &Value {
    match value {
        Value::Object(map) => map.get("js").unwrap_or(value),
        _ => value,
    }
}

fn extract_total_items(value: &Value) -> Option<u32> {
    let js = match value {
        Value::Object(map) => map.get("js").cloned().unwrap_or_else(|| value.clone()),
        other => other.clone(),
    };
    if let Some(obj) = js.as_object() {
        return obj
            .get("total_items")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
    }
    None
}

fn extract_max_page_items(value: &Value) -> Option<u32> {
    let js = match value {
        Value::Object(map) => map.get("js").cloned().unwrap_or_else(|| value.clone()),
        other => other.clone(),
    };
    if let Some(obj) = js.as_object() {
        if let Some(max_items) = obj
            .get("max_page_items")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        {
            return Some(max_items);
        }
        if let Some(max_page) = obj.get("max_page").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()) {
            return Some(max_page);
        }
    }
    None
}

pub async fn get_live_categories(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerCategory>> {
    get_categories(client, handshake, "itv", "get_genres").await
}

pub async fn get_vod_categories(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerCategory>> {
    get_categories(client, handshake, "vod", "get_categories").await
}

pub async fn get_series_categories(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerCategory>> {
    get_categories(client, handshake, "series", "get_categories").await
}

pub async fn get_live_streams_paginated(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerRawItem>> {
    get_paginated_items(client, handshake, "itv", "get_ordered_list", parse_items_page).await
}

pub async fn get_vod_streams_paginated(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerRawItem>> {
    get_paginated_items(client, handshake, "vod", "get_ordered_list", parse_items_page).await
}

pub async fn get_series_list_paginated(client: &StalkerApiClient, handshake: &StalkerHandshake) -> StalkerResult<Vec<StalkerRawSeriesItem>> {
    get_paginated_items(client, handshake, "series", "get_ordered_list", parse_series_page).await
}

pub async fn get_series_details(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    series_id: u32,
) -> StalkerResult<StalkerRawSeriesDetails> {
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        match fetch_series_page(client, handshake, &load_url, spec.token_in_query, series_id, "0").await {
            Ok(value) => match hydrate_series_details(client, handshake, &load_url, spec.token_in_query, series_id, &value).await {
                Ok(details) => return Ok(details),
                Err(err) => last_err = Some(err),
            },
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: client.portal_url().to_string() }))
}

async fn fetch_series_page(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    load_url: &StalkerLoadUrl,
    token_in_query: bool,
    series_id: u32,
    season_id: &str,
) -> StalkerResult<Value> {
    let series_id = series_id.to_string();
    let mut builder = client
        .http()
        .get(&load_url.load_url)
        .headers(client.common_headers(load_url))
        .query(&[
            ("type", "series"),
            ("action", "get_ordered_list"),
            ("JsHttpRequest", "1-xml"),
            ("HttpRequest", "1-xml"),
            ("movie_id", series_id.as_str()),
            ("season_id", season_id),
            ("episode_id", "0"),
        ]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(&handshake.session), token_in_query);
    client.send_json::<Value>(builder, "series_info").await
}

async fn hydrate_series_details(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    load_url: &StalkerLoadUrl,
    token_in_query: bool,
    series_id: u32,
    seed: &Value,
) -> StalkerResult<StalkerRawSeriesDetails> {
    let payload = seed.get("js").unwrap_or(seed);
    if let Ok(details) = serde_json::from_value::<StalkerRawSeriesDetails>(payload.clone()) {
        if !details.seasons.is_empty() {
            return Ok(details);
        }
    }

    let entries = series_entry_values(payload);
    let mut details = series_details_metadata(series_id, &entries);
    let season_rows = season_rows(&entries);

    if season_rows.is_empty() {
        let episodes = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| parse_series_episode(entry, index + 1, 1))
            .collect::<Vec<_>>();
        if !episodes.is_empty() {
            details.seasons.push(StalkerRawSeriesSeason {
                id: Some("1".to_string()),
                number: Some(1),
                name: Some("Season 1".to_string()),
                cover: None,
                episodes,
            });
        }
        return Ok(details);
    }

    for (season_id, row) in season_rows {
        let season_number = value_i32(&row, "season_number")
            .or_else(|| value_i32(&row, "season_id"))
            .or_else(|| season_id.parse::<i32>().ok())
            .unwrap_or(1);
        let page = fetch_series_page(client, handshake, load_url, token_in_query, series_id, &season_id).await?;
        let episode_entries = series_entry_values(page.get("js").unwrap_or(&page));
        let mut episodes = episode_entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| parse_series_episode(entry, index + 1, season_number))
            .collect::<Vec<_>>();
        if episodes.is_empty() {
            episodes = shell_episodes(&row, season_number);
        }
        details.seasons.push(StalkerRawSeriesSeason {
            id: Some(season_id),
            number: Some(season_number),
            name: value_string(&row, "name")
                .or_else(|| value_string(&row, "title"))
                .or_else(|| Some(format!("Season {season_number}"))),
            cover: value_string(&row, "cover").or_else(|| value_string(&row, "screenshot_uri")),
            episodes,
        });
    }
    Ok(details)
}

fn series_entry_values(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(values) => values.clone(),
        Value::Object(object) => {
            if let Some(data) = object.get("data") {
                return series_entry_values(data);
            }
            object.values().filter(|value| value.is_object()).cloned().collect()
        }
        _ => Vec::new(),
    }
}

fn value_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(text) => (!text.trim().is_empty()).then(|| text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn value_i32(value: &Value, key: &str) -> Option<i32> { value_string(value, key)?.parse().ok() }

fn series_details_metadata(series_id: u32, entries: &[Value]) -> StalkerRawSeriesDetails {
    entries.iter().find(|entry| !looks_like_season_row(entry)).map_or_else(
        || StalkerRawSeriesDetails { id: Some(series_id.to_string()), ..Default::default() },
        |entry| StalkerRawSeriesDetails {
            id: value_string(entry, "id").or_else(|| Some(series_id.to_string())),
            name: value_string(entry, "name"),
            title: value_string(entry, "title"),
            plot: value_string(entry, "description").or_else(|| value_string(entry, "plot")),
            cast: value_string(entry, "actors").or_else(|| value_string(entry, "cast")),
            director: value_string(entry, "director"),
            genre: value_string(entry, "genre").or_else(|| value_string(entry, "genres_str")),
            releasedate: value_string(entry, "releasedate").or_else(|| value_string(entry, "year")),
            rating: value_string(entry, "rating").and_then(|value| value.parse().ok()),
            poster: value_string(entry, "screenshot_uri"),
            cover: value_string(entry, "cover"),
            backdrop: None,
            seasons: Vec::new(),
        },
    )
}

fn season_rows(entries: &[Value]) -> Vec<(String, Value)> {
    let mut result = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if !looks_like_season_row(entry) {
            continue;
        }
        let season_id = value_string(entry, "season_id")
            .filter(|value| value != "0")
            .or_else(|| value_string(entry, "season_number"))
            .unwrap_or_else(|| (index + 1).to_string());
        if !result.iter().any(|(existing, _)| existing == &season_id) {
            result.push((season_id, entry.clone()));
        }
    }
    result
}

fn looks_like_season_row(value: &Value) -> bool {
    value_string(value, "season_id").is_some_and(|value| value != "0")
        || value.get("series").and_then(Value::as_array).is_some_and(|values| !values.is_empty())
        || value_string(value, "name")
            .or_else(|| value_string(value, "title"))
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("season "))
}

fn parse_series_episode(value: &Value, fallback_number: usize, fallback_season: i32) -> Option<StalkerRawSeriesEpisode> {
    if looks_like_season_row(value) && value_i32(value, "episode_number").is_none() && value_i32(value, "series_number").is_none() {
        return None;
    }
    let id = value_string(value, "id")
        .or_else(|| value_string(value, "video_id"))
        .or_else(|| value_string(value, "series_id"))?;
    let number = value_i32(value, "series_number")
        .or_else(|| value_i32(value, "episode_number"))
        .or_else(|| i32::try_from(fallback_number).ok());
    Some(StalkerRawSeriesEpisode {
        id: Some(id),
        number,
        name: value_string(value, "name"),
        title: value_string(value, "title").or_else(|| number.map(|number| format!("Episode {number}"))),
        season_number: value_i32(value, "season_number")
            .or_else(|| value_i32(value, "season_id"))
            .or(Some(fallback_season)),
        cmd: value_string(value, "cmd"),
        info: value.get("info").cloned().and_then(|info| serde_json::from_value(info).ok()),
        container_extension: value_string(value, "container_extension"),
    })
}

fn shell_episodes(value: &Value, season_number: i32) -> Vec<StalkerRawSeriesEpisode> {
    value
        .get("series")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let number = entry
                        .as_i64()
                        .and_then(|number| i32::try_from(number).ok())
                        .or_else(|| value_i32(entry, "series_number"))
                        .or_else(|| i32::try_from(index + 1).ok())
                        .unwrap_or(1);
                    StalkerRawSeriesEpisode {
                        id: Some(format!("{}:{number}", value_string(value, "id").unwrap_or_else(|| season_number.to_string()))),
                        number: Some(number),
                        name: Some(format!("Season {season_number} Episode {number}")),
                        season_number: Some(season_number),
                        cmd: value_string(value, "cmd"),
                        ..Default::default()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_items_page(value: &Value) -> (Vec<StalkerRawItem>, Option<u32>, Option<u32>) {
    let arr = extract_items_array(value);
    let items: Vec<StalkerRawItem> = match arr {
        Value::Array(a) => a
            .iter()
            .filter_map(|v| serde_json::from_value::<StalkerRawItem>(v.clone()).ok())
            .collect(),
        Value::Object(obj) => {
            // Some portals use object-keyed data: { "data": { "100": {...}, "101": {...} } }.
            let mut collected: Vec<StalkerRawItem> = Vec::new();
            if let Some(data) = obj.get("data") {
                match data {
                    Value::Array(entries) => {
                        for v in entries {
                            if let Ok(item) = serde_json::from_value::<StalkerRawItem>(v.clone()) {
                                collected.push(item);
                            }
                        }
                    }
                    Value::Object(entries) => {
                        for v in entries.values() {
                            if let Ok(item) = serde_json::from_value::<StalkerRawItem>(v.clone()) {
                                collected.push(item);
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                for v in obj.values() {
                    if let Ok(item) = serde_json::from_value::<StalkerRawItem>(v.clone()) {
                        collected.push(item);
                    }
                }
            }
            collected
        }
        _ => Vec::new(),
    };
    let total = extract_total_items(value);
    let max_page_items = extract_max_page_items(value);
    (items, total, max_page_items)
}

fn parse_series_page(value: &Value) -> (Vec<StalkerRawSeriesItem>, Option<u32>, Option<u32>) {
    let arr = extract_items_array(value);
    let items: Vec<StalkerRawSeriesItem> = match arr {
        Value::Array(a) => a
            .iter()
            .filter_map(|v| serde_json::from_value::<StalkerRawSeriesItem>(v.clone()).ok())
            .collect(),
        Value::Object(obj) => {
            let mut collected: Vec<StalkerRawSeriesItem> = Vec::new();
            if let Some(data) = obj.get("data") {
                match data {
                    Value::Array(entries) => {
                        for v in entries {
                            if let Ok(item) = serde_json::from_value::<StalkerRawSeriesItem>(v.clone()) {
                                collected.push(item);
                            }
                        }
                    }
                    Value::Object(entries) => {
                        for v in entries.values() {
                            if let Ok(item) = serde_json::from_value::<StalkerRawSeriesItem>(v.clone()) {
                                collected.push(item);
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                for v in obj.values() {
                    if let Ok(item) = serde_json::from_value::<StalkerRawSeriesItem>(v.clone()) {
                        collected.push(item);
                    }
                }
            }
            collected
        }
        _ => Vec::new(),
    };
    let total = extract_total_items(value);
    let max_page_items = extract_max_page_items(value);
    (items, total, max_page_items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_from_js_value_parses_minimal() {
        let v: Value = serde_json::from_str(r#"{"id":"42","title":"Movies","alias":"movies","number":1}"#).unwrap();
        let cat = StalkerCategory::from_js_value(&v).expect("ok");
        assert_eq!(cat.id, "42");
        assert_eq!(cat.title, "Movies");
        assert_eq!(cat.alias.as_deref(), Some("movies"));
    }

    #[test]
    fn parse_categories_unwraps_js_wrapper() {
        let v: Value = serde_json::from_str(r#"{"js":[{"id":"1","title":"A"},{"id":"2","title":"B"}]}"#).unwrap();
        let cats = parse_categories(&v);
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0].title, "A");
        assert_eq!(cats[1].title, "B");
    }

    #[test]
    fn parse_items_page_unwraps_object_keyed_data() {
        let v: Value = serde_json::from_str(
            r#"{"js":{"total_items":2,"max_page":1,"data":{"100":{"id":"100","name":"Channel 100"},"101":{"id":"101","name":"Channel 101"}}}}"#,
        )
        .unwrap();
        let (items, total, _max) = parse_items_page(&v);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].display_name(), "Channel 100");
        assert_eq!(total, Some(2));
    }

    #[test]
    fn parse_items_page_handles_array_form() {
        let v: Value = serde_json::from_str(r#"{"js":[{"id":"1","name":"A"}]}"#).unwrap();
        let (items, _, _) = parse_items_page(&v);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_items_page_accepts_boolish_real_portal_fields() {
        let v: Value = serde_json::json!({
            "js": {
                "total_items": 1,
                "max_page_items": 14,
                "data": [{
                    "id": "686148",
                    "name": "QURAN ONE",
                    "number": "1",
                    "cmd": "ffmpeg http://line.tivi-ott.net/live.ts",
                    "enable_tv_archive": 0,
                    "allow_pvr": 0,
                    "allow_local_timeshift": "1",
                    "nginx_secure_link": "0",
                    "wowza_tmp_link": "0",
                    "use_http_tmp_link": "0",
                    "tv_archive_duration": 0
                }]
            }
        });
        let single = serde_json::from_value::<StalkerRawItem>(
            v.get("js").and_then(|js| js.get("data")).and_then(Value::as_array).and_then(|arr| arr.first()).cloned().unwrap()
        );
        assert!(single.is_ok(), "single item decode failed: {single:?}");
        let (items, total, max_page) = parse_items_page(&v);
        assert_eq!(total, Some(1));
        assert_eq!(max_page, Some(14));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.as_deref(), Some("686148"));
        assert_eq!(items[0].allow_local_timeshift, Some(true));
        assert_eq!(items[0].allow_pvr, Some(false));
        assert_eq!(items[0].nginx_secure_link, Some(false));
        assert_eq!(items[0].tv_archive_duration, Some(0));
    }

    #[test]
    fn item_stream_id_parses_when_numeric() {
        let v: Value = serde_json::from_str(r#"{"id":"123","name":"x"}"#).unwrap();
        let item: StalkerRawItem = serde_json::from_value(v).unwrap();
        assert_eq!(item.stream_id(), Some(123));
    }

    #[test]
    fn item_kind_distinguishes_episode() {
        let v: Value = serde_json::from_str(r#"{"id":"1","series_id":"99"}"#).unwrap();
        let item: StalkerRawItem = serde_json::from_value(v).unwrap();
        assert_eq!(item.stream_kind(), StalkerStreamKind::Episode);
    }

    #[test]
    fn season_shells_and_numeric_ids_are_parsed() {
        let value: Value = serde_json::from_str(
            r#"{"data":[{"id":42,"name":"Season 2","season_id":2,"cmd":"encoded","series":[1,2]}]}"#,
        )
        .unwrap();
        let entries = series_entry_values(&value);
        let rows = season_rows(&entries);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "2");
        let episodes = shell_episodes(&rows[0].1, 2);
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[1].number, Some(2));
        assert_eq!(episodes[1].cmd.as_deref(), Some("encoded"));
    }

    #[test]
    fn explicit_episode_row_accepts_numeric_id() {
        let value: Value = serde_json::from_str(
            r#"{"id":77,"name":"Pilot","series_number":1,"season_id":3,"cmd":"encoded"}"#,
        )
        .unwrap();
        let episode = parse_series_episode(&value, 1, 3).expect("episode");
        assert_eq!(episode.id.as_deref(), Some("77"));
        assert_eq!(episode.season_number, Some(3));
    }
}
