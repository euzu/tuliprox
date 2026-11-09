use crate::model::ConfigInput;
use shared::{
    model::{
        FieldDelta, InputType, ItemOverride, LiveStreamProperties, OverrideItemKind, PlaylistItemHeader,
        PlaylistItemType, SeriesStreamProperties, StreamProperties, VideoStreamProperties,
    },
    utils::{extract_id_from_url, extract_numeric_id_from_url, override_url_hash},
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdSource {
    Provider,
    UrlNumeric,
    Attribute,
    UrlHash,
}

#[inline]
fn is_short_hash_shape(s: &str) -> bool { s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()) }

/// Classifies the identity origin of an item from its id, url, and input type.
pub fn classify_id_parts(id: &str, url: &str, input_type: InputType) -> IdSource {
    if !input_type.is_m3u() {
        return IdSource::Provider;
    }
    let numeric_url_id = extract_numeric_id_from_url(url);
    if let Some(num) = numeric_url_id.filter(|&id| id > 0) {
        if id.parse::<u32>().ok() == Some(num) {
            return IdSource::UrlNumeric;
        }
    } else if numeric_url_id == Some(0) && id == "0" {
        return IdSource::UrlHash;
    }

    if is_short_hash_shape(id) && id == extract_id_from_url(url) {
        return IdSource::UrlHash;
    }
    IdSource::Attribute
}

/// Classifies the identity origin of an item header.
pub fn classify_id(header: &PlaylistItemHeader, input_type: InputType) -> IdSource {
    classify_id_parts(&header.id, &header.url, input_type)
}

/// The identity of an item override within a parent input.
///
/// An `OverrideKey` combines the item kind (Live, Vod, Series) with a stable id.
/// The key is unique only within one parent input. Override lookups are scoped
/// per parent input (e.g. `HashMap<Arc<str> /*parent*/, OverrideLookup>`), so items
/// from distinct parent inputs that share the same override key never collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OverrideKey {
    pub kind: OverrideItemKind,
    pub id: Arc<str>,
}

impl OverrideKey {
    pub fn new(kind: OverrideItemKind, id: impl Into<Arc<str>>) -> Self { Self { kind, id: id.into() } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParentInfo {
    parent_name: Arc<str>,
    input_type: InputType,
    credentials: Arc<[Arc<str>]>,
}

#[derive(Debug, Clone, Default)]
pub struct InputAliasMap {
    runtime_to_parent: HashMap<Arc<str>, Arc<ParentInfo>>,
}

fn extract_query_credentials(url: &str, creds: &mut Vec<Arc<str>>) {
    if let Ok(parsed) = Url::parse(url) {
        for (k, v) in parsed.query_pairs() {
            let v_trim = v.trim();
            if (k.eq_ignore_ascii_case("username") || k.eq_ignore_ascii_case("password")) && !v_trim.is_empty() {
                creds.push(Arc::from(v_trim));
            }
        }
    } else if let Some((_path, query)) = url.split_once('?') {
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let v_trim = v.trim();
                if (k.eq_ignore_ascii_case("username") || k.eq_ignore_ascii_case("password")) && !v_trim.is_empty() {
                    creds.push(Arc::from(v_trim));
                }
            }
        }
    }
}

impl InputAliasMap {
    pub fn from_inputs(inputs: &[Arc<ConfigInput>]) -> Self {
        let mut runtime_to_parent = HashMap::new();

        for input in inputs {
            let mut creds: Vec<Arc<str>> = Vec::new();

            if let Some(ref u) = input.username {
                let u_trim = u.trim();
                if !u_trim.is_empty() {
                    creds.push(Arc::from(u_trim));
                }
            }
            if let Some(ref p) = input.password {
                let p_trim = p.trim();
                if !p_trim.is_empty() {
                    creds.push(Arc::from(p_trim));
                }
            }
            extract_query_credentials(&input.url, &mut creds);

            if let Some(ref aliases) = input.aliases {
                for alias in aliases {
                    if let Some(ref u) = alias.username {
                        let u_trim = u.trim();
                        if !u_trim.is_empty() {
                            creds.push(Arc::from(u_trim));
                        }
                    }
                    if let Some(ref p) = alias.password {
                        let p_trim = p.trim();
                        if !p_trim.is_empty() {
                            creds.push(Arc::from(p_trim));
                        }
                    }
                    extract_query_credentials(&alias.url, &mut creds);
                }
            }

            let mut unique_creds = Vec::new();
            for c in creds {
                if !unique_creds.iter().any(|existing: &Arc<str>| existing.as_ref() == c.as_ref()) {
                    unique_creds.push(c);
                }
            }

            let parent_info = Arc::new(ParentInfo {
                parent_name: Arc::clone(&input.name),
                input_type: input.input_type,
                credentials: unique_creds.into(),
            });

            runtime_to_parent.insert(Arc::clone(&input.name), Arc::clone(&parent_info));

            if let Some(ref aliases) = input.aliases {
                for alias in aliases {
                    runtime_to_parent.insert(Arc::clone(&alias.name), Arc::clone(&parent_info));
                }
            }
        }

        Self { runtime_to_parent }
    }

    pub fn parent_of(&self, runtime: &str) -> Option<&str> {
        self.runtime_to_parent.get(runtime).map(|info| info.parent_name.as_ref())
    }

    pub fn credentials_for(&self, runtime: &str) -> Option<&[Arc<str>]> {
        self.runtime_to_parent.get(runtime).map(|info| &info.credentials[..])
    }

    pub fn input_type_of(&self, runtime: &str) -> Option<InputType> {
        self.runtime_to_parent.get(runtime).map(|info| info.input_type)
    }

    pub fn parent_info(&self, runtime: &str) -> Option<(&str, &[Arc<str>])> {
        self.runtime_to_parent.get(runtime).map(|info| (info.parent_name.as_ref(), &info.credentials[..]))
    }
}

/// Computes the override key for an item from its id, url, input name, item type, and alias map.
pub fn override_key_for(
    id: &Arc<str>,
    url: &str,
    input_name: &str,
    item_type: PlaylistItemType,
    aliases: &InputAliasMap,
) -> Option<OverrideKey> {
    if id.is_empty() {
        return None;
    }
    let parent_info = aliases.runtime_to_parent.get(input_name)?;

    let id_source = classify_id_parts(id, url, parent_info.input_type);
    let key_id = if id_source == IdSource::UrlHash {
        let Some(hash) = override_url_hash(url, &parent_info.credentials[..]) else {
            log::debug!("Failed to compute canonical override url hash for item {id}");
            return None;
        };
        Arc::from(hash.as_str())
    } else {
        Arc::clone(id)
    };

    let kind = OverrideItemKind::from(item_type);
    Some(OverrideKey { kind, id: key_id })
}

/// Computes the override key for a playlist item header.
pub fn override_key(header: &PlaylistItemHeader, aliases: &InputAliasMap) -> Option<OverrideKey> {
    override_key_for(&header.id, &header.url, &header.input_name, header.item_type, aliases)
}

#[derive(Debug, Clone, Default)]
pub struct OverrideLookup {
    pub map: HashMap<OverrideKey, Arc<ItemOverride>>,
}

impl OverrideLookup {
    pub fn new(map: HashMap<OverrideKey, Arc<ItemOverride>>) -> Self { Self { map } }

    pub fn get(&self, key: &OverrideKey) -> Option<&Arc<ItemOverride>> { self.map.get(key) }

    pub fn is_empty(&self) -> bool { self.map.is_empty() }

    pub fn len(&self) -> usize { self.map.len() }
}

impl From<HashMap<OverrideKey, Arc<ItemOverride>>> for OverrideLookup {
    fn from(map: HashMap<OverrideKey, Arc<ItemOverride>>) -> Self { Self { map } }
}

macro_rules! apply_str_delta {
    ($target:expr, $delta:expr) => {
        match &$delta {
            FieldDelta::Keep => {}
            FieldDelta::Clear => {
                $target = Arc::from("");
            }
            FieldDelta::Set(v) => {
                $target = Arc::from(v.as_str());
            }
        }
    };
}

fn apply_header_fields(header: &mut PlaylistItemHeader, ov: &ItemOverride, skip_chno: bool) {
    apply_str_delta!(header.name, ov.name);
    apply_str_delta!(header.title, ov.title);
    apply_str_delta!(header.group, ov.group);
    apply_str_delta!(header.logo, ov.logo);
    apply_str_delta!(header.logo_small, ov.logo_small);
    apply_str_delta!(header.parent_code, ov.parent_code);
    apply_str_delta!(header.audio_track, ov.audio_track);
    apply_str_delta!(header.time_shift, ov.time_shift);
    apply_str_delta!(header.rec, ov.rec);

    if !skip_chno {
        match ov.chno {
            FieldDelta::Keep => {}
            FieldDelta::Clear => header.chno = 0,
            FieldDelta::Set(v) => header.chno = v,
        }
    }

    match &ov.epg_channel_id {
        FieldDelta::Keep => {}
        FieldDelta::Clear => header.epg_channel_id = None,
        FieldDelta::Set(v) => header.epg_channel_id = Some(Arc::from(v.as_str())),
    }
}

fn apply_live_properties(live: &mut LiveStreamProperties, ov: &ItemOverride) {
    apply_str_delta!(live.name, ov.name);
    apply_str_delta!(live.stream_icon, ov.logo);
    match &ov.epg_channel_id {
        FieldDelta::Keep => {}
        FieldDelta::Clear => live.epg_channel_id = None,
        FieldDelta::Set(v) => live.epg_channel_id = Some(Arc::from(v.as_str())),
    }
}

fn apply_video_properties(video: &mut VideoStreamProperties, ov: &ItemOverride) {
    apply_str_delta!(video.name, ov.name);
    match &ov.logo {
        FieldDelta::Keep => {}
        FieldDelta::Clear => {
            video.stream_icon = Arc::from("");
            if let Some(details) = video.details.as_mut() {
                details.movie_image = None;
                details.cover_big = None;
                details.backdrop_path = None;
            }
        }
        FieldDelta::Set(v) => {
            let val: Arc<str> = Arc::from(v.as_str());
            video.stream_icon = Arc::clone(&val);
            if let Some(details) = video.details.as_mut() {
                details.movie_image = Some(Arc::clone(&val));
                details.cover_big = Some(val);
            }
        }
    }

    match ov.tmdb {
        FieldDelta::Keep => {}
        FieldDelta::Clear => video.tmdb = None,
        FieldDelta::Set(v) => video.tmdb = Some(v),
    }
    match ov.rating {
        FieldDelta::Keep => {}
        FieldDelta::Clear => {
            video.rating = None;
            video.rating_5based = None;
        }
        FieldDelta::Set(v) => {
            video.rating = Some(v);
            video.rating_5based = Some(v / 2.0);
        }
    }

    let has_detail_override = !ov.plot.is_keep()
        || !ov.genre.is_keep()
        || !ov.cast.is_keep()
        || !ov.director.is_keep()
        || !ov.release_date.is_keep();

    if has_detail_override {
        let details = video.details.get_or_insert_with(Default::default);
        match &ov.plot {
            FieldDelta::Keep => {}
            FieldDelta::Clear => details.plot = None,
            FieldDelta::Set(v) => details.plot = Some(Arc::from(v.as_str())),
        }
        match &ov.genre {
            FieldDelta::Keep => {}
            FieldDelta::Clear => details.genre = None,
            FieldDelta::Set(v) => details.genre = Some(Arc::from(v.as_str())),
        }
        match &ov.cast {
            FieldDelta::Keep => {}
            FieldDelta::Clear => details.cast = None,
            FieldDelta::Set(v) => details.cast = Some(Arc::from(v.as_str())),
        }
        match &ov.director {
            FieldDelta::Keep => {}
            FieldDelta::Clear => details.director = None,
            FieldDelta::Set(v) => details.director = Some(Arc::from(v.as_str())),
        }
        match &ov.release_date {
            FieldDelta::Keep => {}
            FieldDelta::Clear => details.release_date = None,
            FieldDelta::Set(v) => details.release_date = Some(Arc::from(v.as_str())),
        }
    }
}

fn apply_series_properties(series: &mut SeriesStreamProperties, ov: &ItemOverride) {
    apply_str_delta!(series.name, ov.name);
    match &ov.logo {
        FieldDelta::Keep => {}
        FieldDelta::Clear => {
            series.cover = Arc::from("");
            series.backdrop_path = None;
        }
        FieldDelta::Set(v) => {
            series.cover = Arc::from(v.as_str());
        }
    }

    match ov.tmdb {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.tmdb = None,
        FieldDelta::Set(v) => series.tmdb = Some(v),
    }
    match ov.rating {
        FieldDelta::Keep => {}
        FieldDelta::Clear => {
            series.rating = 0.0;
            series.rating_5based = 0.0;
        }
        FieldDelta::Set(v) => {
            series.rating = v;
            series.rating_5based = v / 2.0;
        }
    }
    match &ov.plot {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.plot = None,
        FieldDelta::Set(v) => series.plot = Some(Arc::from(v.as_str())),
    }
    match &ov.genre {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.genre = None,
        FieldDelta::Set(v) => series.genre = Some(Arc::from(v.as_str())),
    }
    match &ov.cast {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.cast = Arc::from(""),
        FieldDelta::Set(v) => series.cast = Arc::from(v.as_str()),
    }
    match &ov.director {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.director = Arc::from(""),
        FieldDelta::Set(v) => series.director = Arc::from(v.as_str()),
    }
    match &ov.release_date {
        FieldDelta::Keep => {}
        FieldDelta::Clear => series.release_date = None,
        FieldDelta::Set(v) => series.release_date = Some(Arc::from(v.as_str())),
    }
}

fn apply_properties(header: &mut PlaylistItemHeader, ov: &ItemOverride) {
    match &mut header.additional_properties {
        Some(StreamProperties::Live(live)) => apply_live_properties(live, ov),
        Some(StreamProperties::Video(video)) => apply_video_properties(video, ov),
        Some(StreamProperties::Series(series)) => apply_series_properties(series, ov),
        other => {
            let has_metadata = !ov.tmdb.is_keep()
                || !ov.plot.is_keep()
                || !ov.genre.is_keep()
                || !ov.cast.is_keep()
                || !ov.director.is_keep()
                || !ov.release_date.is_keep()
                || !ov.rating.is_keep();

            if has_metadata {
                log::debug!(
                    "Skipping metadata override fields for item {}: additional_properties is {:?}",
                    header.id,
                    other.as_ref().map(|p| match p {
                        StreamProperties::Episode(_) => "Episode",
                        _ => "Unknown",
                    })
                );
            }
        }
    }
}

pub fn apply_override(header: &mut PlaylistItemHeader, ov: &ItemOverride) { apply_override_ext(header, ov, false); }

pub fn apply_override_skip_chno(header: &mut PlaylistItemHeader, ov: &ItemOverride) {
    apply_override_ext(header, ov, true);
}

pub fn apply_override_ext(header: &mut PlaylistItemHeader, ov: &ItemOverride, skip_chno: bool) {
    apply_header_fields(header, ov, skip_chno);
    apply_properties(header, ov);
}

#[cfg(test)]
mod tests;
