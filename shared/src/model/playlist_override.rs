use crate::model::PlaylistItemType;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

pub const OVERRIDE_SCHEMA_VERSION: u32 = 1;

/// Represents a delta state for a field:
/// - Keep: leave current value untouched (absent in YAML/JSON)
/// - Clear: reset field to empty/default (serialized as `null`)
/// - Set(T): overwrite with a new value
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FieldDelta<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> FieldDelta<T> {
    #[inline]
    pub fn is_keep(&self) -> bool { matches!(self, Self::Keep) }

    #[inline]
    pub fn is_clear(&self) -> bool { matches!(self, Self::Clear) }

    #[inline]
    pub fn is_set(&self) -> bool { matches!(self, Self::Set(_)) }

    #[inline]
    pub fn as_set(&self) -> Option<&T> {
        match self {
            Self::Set(v) => Some(v),
            _ => None,
        }
    }
}

impl<T: Serialize> Serialize for FieldDelta<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Keep | Self::Clear => serializer.serialize_none(),
            Self::Set(v) => v.serialize(serializer),
        }
    }
}

pub fn deserialize_field_delta<'de, D, T>(deserializer: D) -> Result<FieldDelta<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|opt| match opt {
        Some(v) => FieldDelta::Set(v),
        None => FieldDelta::Clear,
    })
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for FieldDelta<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_field_delta(deserializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideItemKind {
    Live,
    Vod,
    Series,
}

impl From<PlaylistItemType> for OverrideItemKind {
    fn from(item_type: PlaylistItemType) -> Self {
        if item_type.is_video() {
            Self::Vod
        } else if item_type.is_series() || item_type.is_series_info() {
            Self::Series
        } else {
            Self::Live
        }
    }
}

/// A manual per-item override containing field deltas.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemOverride {
    pub hint: Option<String>,
    pub audio_track: FieldDelta<String>,
    pub cast: FieldDelta<String>,
    pub chno: FieldDelta<u32>,
    pub director: FieldDelta<String>,
    pub epg_channel_id: FieldDelta<String>,
    pub genre: FieldDelta<String>,
    pub group: FieldDelta<String>,
    pub logo: FieldDelta<String>,
    pub logo_small: FieldDelta<String>,
    pub name: FieldDelta<String>,
    pub parent_code: FieldDelta<String>,
    pub plot: FieldDelta<String>,
    pub rating: FieldDelta<f64>,
    pub rec: FieldDelta<String>,
    pub release_date: FieldDelta<String>,
    pub time_shift: FieldDelta<String>,
    pub title: FieldDelta<String>,
    pub tmdb: FieldDelta<u32>,
    pub unknown: BTreeMap<String, serde_json::Value>,
}

impl Serialize for ItemOverride {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map: BTreeMap<&str, serde_json::Value> = BTreeMap::new();

        macro_rules! insert_delta {
            ($name:literal, $delta:expr) => {
                match &$delta {
                    FieldDelta::Keep => {}
                    FieldDelta::Clear => {
                        map.insert($name, serde_json::Value::Null);
                    }
                    FieldDelta::Set(v) => {
                        if let Ok(val) = serde_json::to_value(v) {
                            map.insert($name, val);
                        }
                    }
                }
            };
        }

        insert_delta!("audio_track", self.audio_track);
        insert_delta!("cast", self.cast);
        insert_delta!("chno", self.chno);
        insert_delta!("director", self.director);
        insert_delta!("epg_channel_id", self.epg_channel_id);
        insert_delta!("genre", self.genre);
        insert_delta!("group", self.group);
        insert_delta!("logo", self.logo);
        insert_delta!("logo_small", self.logo_small);
        insert_delta!("name", self.name);
        insert_delta!("parent_code", self.parent_code);
        insert_delta!("plot", self.plot);
        insert_delta!("rating", self.rating);
        insert_delta!("rec", self.rec);
        insert_delta!("release_date", self.release_date);
        insert_delta!("time_shift", self.time_shift);
        insert_delta!("title", self.title);
        insert_delta!("tmdb", self.tmdb);

        for (k, v) in &self.unknown {
            map.insert(k.as_str(), v.clone());
        }

        let total = map.len() + usize::from(self.hint.is_some());
        let mut map_ser = serializer.serialize_map(Some(total))?;

        if let Some(hint) = &self.hint {
            map_ser.serialize_entry("_hint", hint)?;
        }

        for (k, v) in &map {
            map_ser.serialize_entry(k, v)?;
        }

        map_ser.end()
    }
}

impl ItemOverride {
    pub fn is_empty(&self) -> bool {
        self.audio_track.is_keep()
            && self.cast.is_keep()
            && self.chno.is_keep()
            && self.director.is_keep()
            && self.epg_channel_id.is_keep()
            && self.genre.is_keep()
            && self.group.is_keep()
            && self.logo.is_keep()
            && self.logo_small.is_keep()
            && self.name.is_keep()
            && self.parent_code.is_keep()
            && self.plot.is_keep()
            && self.rating.is_keep()
            && self.rec.is_keep()
            && self.release_date.is_keep()
            && self.time_shift.is_keep()
            && self.title.is_keep()
            && self.tmdb.is_keep()
            && self.unknown.is_empty()
    }

    pub fn apply_patch(&mut self, patch: &ItemOverridePatch) {
        apply_field_op(&mut self.audio_track, &patch.audio_track);
        apply_field_op(&mut self.cast, &patch.cast);
        apply_field_op(&mut self.chno, &patch.chno);
        apply_field_op(&mut self.director, &patch.director);
        apply_field_op(&mut self.epg_channel_id, &patch.epg_channel_id);
        apply_field_op(&mut self.genre, &patch.genre);
        apply_field_op(&mut self.group, &patch.group);
        apply_field_op(&mut self.logo, &patch.logo);
        apply_field_op(&mut self.logo_small, &patch.logo_small);
        apply_field_op(&mut self.name, &patch.name);
        apply_field_op(&mut self.parent_code, &patch.parent_code);
        apply_field_op(&mut self.plot, &patch.plot);
        apply_field_op(&mut self.rating, &patch.rating);
        apply_field_op(&mut self.rec, &patch.rec);
        apply_field_op(&mut self.release_date, &patch.release_date);
        apply_field_op(&mut self.time_shift, &patch.time_shift);
        apply_field_op(&mut self.title, &patch.title);
        apply_field_op(&mut self.tmdb, &patch.tmdb);

        if patch.audio_track.is_some() {
            self.unknown.remove("audio_track");
        }
        if patch.cast.is_some() {
            self.unknown.remove("cast");
        }
        if patch.chno.is_some() {
            self.unknown.remove("chno");
        }
        if patch.director.is_some() {
            self.unknown.remove("director");
        }
        if patch.epg_channel_id.is_some() {
            self.unknown.remove("epg_channel_id");
        }
        if patch.genre.is_some() {
            self.unknown.remove("genre");
        }
        if patch.group.is_some() {
            self.unknown.remove("group");
        }
        if patch.logo.is_some() {
            self.unknown.remove("logo");
        }
        if patch.logo_small.is_some() {
            self.unknown.remove("logo_small");
        }
        if patch.name.is_some() {
            self.unknown.remove("name");
        }
        if patch.parent_code.is_some() {
            self.unknown.remove("parent_code");
        }
        if patch.plot.is_some() {
            self.unknown.remove("plot");
        }
        if patch.rating.is_some() {
            self.unknown.remove("rating");
        }
        if patch.rec.is_some() {
            self.unknown.remove("rec");
        }
        if patch.release_date.is_some() {
            self.unknown.remove("release_date");
        }
        if patch.time_shift.is_some() {
            self.unknown.remove("time_shift");
        }
        if patch.title.is_some() {
            self.unknown.remove("title");
        }
        if patch.tmdb.is_some() {
            self.unknown.remove("tmdb");
        }

        for key in &patch.remove_unknown {
            self.unknown.remove(key);
        }
    }

    pub fn validate(&self, kind: OverrideItemKind) -> Vec<FieldError> {
        let mut errors = Vec::new();

        match kind {
            OverrideItemKind::Live => {
                if !self.tmdb.is_keep() {
                    errors.push(FieldError::new("tmdb", "error.field_not_allowed"));
                }
                if !self.plot.is_keep() {
                    errors.push(FieldError::new("plot", "error.field_not_allowed"));
                }
                if !self.genre.is_keep() {
                    errors.push(FieldError::new("genre", "error.field_not_allowed"));
                }
                if !self.cast.is_keep() {
                    errors.push(FieldError::new("cast", "error.field_not_allowed"));
                }
                if !self.director.is_keep() {
                    errors.push(FieldError::new("director", "error.field_not_allowed"));
                }
                if !self.release_date.is_keep() {
                    errors.push(FieldError::new("release_date", "error.field_not_allowed"));
                }
                if !self.rating.is_keep() {
                    errors.push(FieldError::new("rating", "error.field_not_allowed"));
                }

                if let FieldDelta::Set(v) = &self.logo_small {
                    if !is_valid_http_url(v) {
                        errors.push(FieldError::new("logo_small", "error.invalid_url"));
                    }
                }
                if let FieldDelta::Set(v) = &self.epg_channel_id {
                    if v.trim().is_empty() {
                        errors.push(FieldError::new("epg_channel_id", "error.invalid_epg_channel_id"));
                    }
                }
                if let FieldDelta::Set(v) = &self.chno {
                    if !(1..=999_999).contains(v) {
                        errors.push(FieldError::new("chno", "error.invalid_channel_no"));
                    }
                }
            }
            OverrideItemKind::Vod | OverrideItemKind::Series => {
                if !self.logo_small.is_keep() {
                    errors.push(FieldError::new("logo_small", "error.field_not_allowed"));
                }
                if !self.epg_channel_id.is_keep() {
                    errors.push(FieldError::new("epg_channel_id", "error.field_not_allowed"));
                }
                if !self.chno.is_keep() {
                    errors.push(FieldError::new("chno", "error.field_not_allowed"));
                }
                if !self.parent_code.is_keep() {
                    errors.push(FieldError::new("parent_code", "error.field_not_allowed"));
                }
                if !self.audio_track.is_keep() {
                    errors.push(FieldError::new("audio_track", "error.field_not_allowed"));
                }
                if !self.time_shift.is_keep() {
                    errors.push(FieldError::new("time_shift", "error.field_not_allowed"));
                }
                if !self.rec.is_keep() {
                    errors.push(FieldError::new("rec", "error.field_not_allowed"));
                }

                if let FieldDelta::Set(v) = &self.tmdb {
                    if *v == 0 {
                        errors.push(FieldError::new("tmdb", "error.invalid_tmdb"));
                    }
                }
                if let FieldDelta::Set(v) = &self.release_date {
                    if !is_valid_date(v) {
                        errors.push(FieldError::new("release_date", "error.invalid_date"));
                    }
                }
                if let FieldDelta::Set(v) = &self.rating {
                    if !(0.0..=10.0).contains(v) || v.is_nan() {
                        errors.push(FieldError::new("rating", "error.invalid_rating"));
                    }
                }
            }
        }

        // name, title, group: clear is not allowed
        if self.name.is_clear() {
            errors.push(FieldError::new("name", "error.cannot_clear"));
        } else if let FieldDelta::Set(v) = &self.name {
            if v.is_empty() || v.trim() != v {
                errors.push(FieldError::new("name", "error.invalid_name"));
            }
        }

        if self.title.is_clear() {
            errors.push(FieldError::new("title", "error.cannot_clear"));
        } else if let FieldDelta::Set(v) = &self.title {
            if v.is_empty() || v.trim() != v {
                errors.push(FieldError::new("title", "error.invalid_title"));
            }
        }

        if self.group.is_clear() {
            errors.push(FieldError::new("group", "error.cannot_clear"));
        } else if let FieldDelta::Set(v) = &self.group {
            if v.is_empty() || v.trim() != v {
                errors.push(FieldError::new("group", "error.invalid_group"));
            }
        }

        if let FieldDelta::Set(v) = &self.logo {
            if !is_valid_http_url(v) {
                errors.push(FieldError::new("logo", "error.invalid_url"));
            }
        }

        errors
    }
}

pub fn validate(item: &ItemOverride, kind: OverrideItemKind) -> Vec<FieldError> { item.validate(kind) }

pub fn apply_patch(item: &mut ItemOverride, patch: &ItemOverridePatch) { item.apply_patch(patch); }

fn is_valid_http_url(s: &str) -> bool {
    let Ok(u) = url::Url::parse(s) else {
        return false;
    };
    (u.scheme() == "http" || u.scheme() == "https")
        && (s.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || s.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://")))
        && u.has_host()
        && u.host_str().is_some_and(|h| !h.is_empty())
}

fn is_valid_date(s: &str) -> bool { chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub field: String,
    pub message_key: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message_key: impl Into<String>) -> Self {
        Self { entry: None, field: field.into(), message_key: message_key.into() }
    }

    pub fn with_entry(mut self, entry: impl Into<String>) -> Self {
        self.entry = Some(entry.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideFieldOp<T> {
    Set(T),
    Clear,
    Reset,
}

impl<T: Serialize> Serialize for OverrideFieldOp<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum Helper<'a, T> {
            Set { value: &'a T },
            Clear,
            Reset,
        }

        let helper = match self {
            Self::Set(v) => Helper::Set { value: v },
            Self::Clear => Helper::Clear,
            Self::Reset => Helper::Reset,
        };
        helper.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OverrideFieldOp<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum Helper<T> {
            Set { value: T },
            Clear,
            Reset,
        }

        match Helper::deserialize(deserializer)? {
            Helper::Set { value } => Ok(Self::Set(value)),
            Helper::Clear => Ok(Self::Clear),
            Helper::Reset => Ok(Self::Reset),
        }
    }
}

pub fn deserialize_patch_op<'de, D, T>(deserializer: D) -> Result<Option<OverrideFieldOp<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    OverrideFieldOp::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ItemOverridePatch {
    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub audio_track: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub cast: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub chno: Option<OverrideFieldOp<u32>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub director: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub genre: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub group: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub logo: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub logo_small: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub name: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub parent_code: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub plot: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub rating: Option<OverrideFieldOp<f64>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub rec: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub release_date: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub time_shift: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub title: Option<OverrideFieldOp<String>>,

    #[serde(default, deserialize_with = "deserialize_patch_op", skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<OverrideFieldOp<u32>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_unknown: Vec<String>,
}

fn apply_field_op<T: Clone>(delta: &mut FieldDelta<T>, op: &Option<OverrideFieldOp<T>>) {
    if let Some(op) = op {
        match op {
            OverrideFieldOp::Set(val) => *delta = FieldDelta::Set(val.clone()),
            OverrideFieldOp::Clear => *delta = FieldDelta::Clear,
            OverrideFieldOp::Reset => *delta = FieldDelta::Keep,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct InputOverrideSections {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub live: BTreeMap<String, ItemOverride>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vod: BTreeMap<String, ItemOverride>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub series: BTreeMap<String, ItemOverride>,
}

impl InputOverrideSections {
    pub fn prune_empty(&mut self) {
        self.live.retain(|_, item| !item.is_empty());
        self.vod.retain(|_, item| !item.is_empty());
        self.series.retain(|_, item| !item.is_empty());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputOverrideFile {
    pub schema_version: u32,
    #[serde(flatten)]
    pub sections: InputOverrideSections,
}

impl Default for InputOverrideFile {
    fn default() -> Self {
        Self { schema_version: OVERRIDE_SCHEMA_VERSION, sections: InputOverrideSections::default() }
    }
}

impl std::ops::Deref for InputOverrideFile {
    type Target = InputOverrideSections;

    fn deref(&self) -> &Self::Target { &self.sections }
}

impl std::ops::DerefMut for InputOverrideFile {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.sections }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetOverrideFile {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputOverrideSections>,
}

impl Default for TargetOverrideFile {
    fn default() -> Self { Self { schema_version: OVERRIDE_SCHEMA_VERSION, inputs: BTreeMap::new() } }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverrideParseError {
    #[error("Malformed override file: {0}")]
    Malformed(String),
    #[error("Unsupported schema version: {0}")]
    UnsupportedSchema(u32),
}

fn parse_string_field(
    delta: &mut FieldDelta<String>,
    entry: &str,
    key: &str,
    val: &serde_json::Value,
    errors: &mut Vec<FieldError>,
    unknown: &mut BTreeMap<String, serde_json::Value>,
) {
    match val {
        serde_json::Value::Null => *delta = FieldDelta::Clear,
        serde_json::Value::String(s) => *delta = FieldDelta::Set(s.clone()),
        _ => {
            errors.push(FieldError::new(key, "error.invalid_type").with_entry(entry));
            unknown.insert(key.to_string(), val.clone());
        }
    }
}

fn parse_u32_field(
    delta: &mut FieldDelta<u32>,
    entry: &str,
    key: &str,
    val: &serde_json::Value,
    errors: &mut Vec<FieldError>,
    unknown: &mut BTreeMap<String, serde_json::Value>,
) {
    match val {
        serde_json::Value::Null => *delta = FieldDelta::Clear,
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if let Ok(u) = u32::try_from(u) {
                    *delta = FieldDelta::Set(u);
                    return;
                }
            }
            errors.push(FieldError::new(key, "error.invalid_type").with_entry(entry));
            unknown.insert(key.to_string(), val.clone());
        }
        _ => {
            errors.push(FieldError::new(key, "error.invalid_type").with_entry(entry));
            unknown.insert(key.to_string(), val.clone());
        }
    }
}

fn parse_f64_field(
    delta: &mut FieldDelta<f64>,
    entry: &str,
    key: &str,
    val: &serde_json::Value,
    errors: &mut Vec<FieldError>,
    unknown: &mut BTreeMap<String, serde_json::Value>,
) {
    match val {
        serde_json::Value::Null => *delta = FieldDelta::Clear,
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                *delta = FieldDelta::Set(f);
            } else {
                errors.push(FieldError::new(key, "error.invalid_type").with_entry(entry));
                unknown.insert(key.to_string(), val.clone());
            }
        }
        _ => {
            errors.push(FieldError::new(key, "error.invalid_type").with_entry(entry));
            unknown.insert(key.to_string(), val.clone());
        }
    }
}

fn parse_item_override(val: &serde_json::Value, entry: &str, errors: &mut Vec<FieldError>) -> ItemOverride {
    let mut item = ItemOverride::default();
    let Some(map) = val.as_object() else {
        return item;
    };

    for (k, v) in map {
        match k.as_str() {
            "_hint" => {
                if let Some(s) = v.as_str() {
                    item.hint = Some(s.to_string());
                } else if !v.is_null() {
                    errors.push(FieldError::new("_hint", "error.invalid_type").with_entry(entry));
                    item.unknown.insert("_hint".to_string(), v.clone());
                }
            }
            "audio_track" => parse_string_field(&mut item.audio_track, entry, k, v, errors, &mut item.unknown),
            "cast" => parse_string_field(&mut item.cast, entry, k, v, errors, &mut item.unknown),
            "chno" => parse_u32_field(&mut item.chno, entry, k, v, errors, &mut item.unknown),
            "director" => parse_string_field(&mut item.director, entry, k, v, errors, &mut item.unknown),
            "epg_channel_id" => parse_string_field(&mut item.epg_channel_id, entry, k, v, errors, &mut item.unknown),
            "genre" => parse_string_field(&mut item.genre, entry, k, v, errors, &mut item.unknown),
            "group" => parse_string_field(&mut item.group, entry, k, v, errors, &mut item.unknown),
            "logo" => parse_string_field(&mut item.logo, entry, k, v, errors, &mut item.unknown),
            "logo_small" => parse_string_field(&mut item.logo_small, entry, k, v, errors, &mut item.unknown),
            "name" => parse_string_field(&mut item.name, entry, k, v, errors, &mut item.unknown),
            "parent_code" => parse_string_field(&mut item.parent_code, entry, k, v, errors, &mut item.unknown),
            "plot" => parse_string_field(&mut item.plot, entry, k, v, errors, &mut item.unknown),
            "rating" => parse_f64_field(&mut item.rating, entry, k, v, errors, &mut item.unknown),
            "rec" => parse_string_field(&mut item.rec, entry, k, v, errors, &mut item.unknown),
            "release_date" => parse_string_field(&mut item.release_date, entry, k, v, errors, &mut item.unknown),
            "time_shift" => parse_string_field(&mut item.time_shift, entry, k, v, errors, &mut item.unknown),
            "title" => parse_string_field(&mut item.title, entry, k, v, errors, &mut item.unknown),
            "tmdb" => parse_u32_field(&mut item.tmdb, entry, k, v, errors, &mut item.unknown),
            unknown_key => {
                errors.push(FieldError::new(unknown_key, "error.unknown_field").with_entry(entry));
                item.unknown.insert(unknown_key.to_string(), v.clone());
            }
        }
    }

    item
}

const ALLOWED_INPUT_ROOT_KEYS: &[&str] = &["schema_version", "live", "vod", "series"];
const SECTIONS: &[&str] = &["live", "vod", "series"];
const ALLOWED_TARGET_ROOT_KEYS: &[&str] = &["schema_version", "inputs"];

fn extract_schema_version(obj: &serde_json::Map<String, serde_json::Value>) -> Result<u32, OverrideParseError> {
    let val =
        obj.get("schema_version").ok_or_else(|| OverrideParseError::Malformed("missing schema_version".to_string()))?;

    let Some(n) = val.as_number() else {
        return Err(OverrideParseError::Malformed("schema_version must be an integer".to_string()));
    };

    if let Some(u) = n.as_u64() {
        let version = u32::try_from(u).map_err(|_| OverrideParseError::UnsupportedSchema(u32::MAX))?;
        if version != OVERRIDE_SCHEMA_VERSION {
            return Err(OverrideParseError::UnsupportedSchema(version));
        }
        Ok(version)
    } else if n.as_i64().is_some() {
        Err(OverrideParseError::UnsupportedSchema(u32::MAX))
    } else {
        Err(OverrideParseError::Malformed("schema_version must be an integer".to_string()))
    }
}

pub fn parse_input_override_file(yaml: &str) -> Result<(InputOverrideFile, Vec<FieldError>), OverrideParseError> {
    let value: serde_json::Value =
        crate::utils::yaml_from_str(yaml).map_err(|e| OverrideParseError::Malformed(e.to_string()))?;

    let obj = value.as_object().ok_or_else(|| OverrideParseError::Malformed("expected mapping at root".to_string()))?;

    for key in obj.keys() {
        if !ALLOWED_INPUT_ROOT_KEYS.contains(&key.as_str()) {
            return Err(OverrideParseError::Malformed(format!("unknown root key '{key}'")));
        }
    }

    let schema_version = extract_schema_version(obj)?;

    let mut errors = Vec::new();
    let mut sections = InputOverrideSections::default();

    for &sec in SECTIONS {
        if let Some(sec_val) = obj.get(sec) {
            let Some(sec_obj) = sec_val.as_object() else {
                return Err(OverrideParseError::Malformed(format!("{sec}: expected mapping")));
            };
            let mut sec_items = BTreeMap::new();
            for (id, item_val) in sec_obj {
                if !item_val.is_object() {
                    return Err(OverrideParseError::Malformed(format!("{sec}.{id}: expected mapping")));
                }
                let entry_path = format!("{sec}.{id}");
                let item = parse_item_override(item_val, &entry_path, &mut errors);
                sec_items.insert(id.clone(), item);
            }
            match sec {
                "live" => sections.live = sec_items,
                "vod" => sections.vod = sec_items,
                "series" => sections.series = sec_items,
                _ => {}
            }
        }
    }

    Ok((InputOverrideFile { schema_version, sections }, errors))
}

pub fn parse_target_override_file(yaml: &str) -> Result<(TargetOverrideFile, Vec<FieldError>), OverrideParseError> {
    let value: serde_json::Value =
        crate::utils::yaml_from_str(yaml).map_err(|e| OverrideParseError::Malformed(e.to_string()))?;

    let obj = value.as_object().ok_or_else(|| OverrideParseError::Malformed("expected mapping at root".to_string()))?;

    for key in obj.keys() {
        if !ALLOWED_TARGET_ROOT_KEYS.contains(&key.as_str()) {
            return Err(OverrideParseError::Malformed(format!("unknown root key '{key}'")));
        }
    }

    let schema_version = extract_schema_version(obj)?;

    let mut errors = Vec::new();
    let mut inputs = BTreeMap::new();

    if let Some(inputs_val) = obj.get("inputs") {
        let Some(inputs_obj) = inputs_val.as_object() else {
            return Err(OverrideParseError::Malformed("inputs: expected mapping".to_string()));
        };
        for (input_name, input_val) in inputs_obj {
            let Some(sections_obj) = input_val.as_object() else {
                return Err(OverrideParseError::Malformed(format!("inputs.{input_name}: expected mapping")));
            };
            let mut sections = InputOverrideSections::default();
            for (sec_name, sec_val) in sections_obj {
                match sec_name.as_str() {
                    "live" | "vod" | "series" => {
                        let Some(sec_map) = sec_val.as_object() else {
                            return Err(OverrideParseError::Malformed(format!(
                                "inputs.{input_name}.{sec_name}: expected mapping"
                            )));
                        };
                        let mut sec_items = BTreeMap::new();
                        for (id, item_val) in sec_map {
                            if !item_val.is_object() {
                                return Err(OverrideParseError::Malformed(format!(
                                    "inputs.{input_name}.{sec_name}.{id}: expected mapping"
                                )));
                            }
                            let entry_path = format!("{input_name}.{sec_name}.{id}");
                            let item = parse_item_override(item_val, &entry_path, &mut errors);
                            sec_items.insert(id.clone(), item);
                        }
                        match sec_name.as_str() {
                            "live" => sections.live = sec_items,
                            "vod" => sections.vod = sec_items,
                            "series" => sections.series = sec_items,
                            _ => {}
                        }
                    }
                    unknown => {
                        return Err(OverrideParseError::Malformed(format!(
                            "inputs.{input_name}: unknown section '{unknown}'"
                        )));
                    }
                }
            }
            inputs.insert(input_name.clone(), sections);
        }
    }

    Ok((TargetOverrideFile { schema_version, inputs }, errors))
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------------------------
// Override API DTOs.
//
// The browser never uploads a whole override file: it sends field-level tagged operations for
// one or more items and always carries the file revision it based the change on.
// ---------------------------------------------------------------------------------------------

/// Override file kind as named on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideScopeDto {
    Input,
    Target,
}

impl OverrideScopeDto {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Target => "target",
        }
    }
}

impl std::fmt::Display for OverrideScopeDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

/// One stored override entry of a file, as the Explorer lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideEntryDto {
    /// Parent input of the entry; `None` inside an input file, always set inside a target file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub kind: OverrideItemKind,
    pub id: String,
    /// Informational `"<group> / <title>"` recorded when the entry was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Stored deltas: every set field with its value, `null` for a cleared field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, serde_json::Value>,
}

/// Content of one override file plus whatever could not be parsed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OverrideFileDto {
    /// Content hash the next save has to be based on.
    pub revision: String,
    #[serde(default)]
    pub entries: Vec<OverrideEntryDto>,
    /// Field-level problems the stored file still carries; the file is never rewritten for them.
    #[serde(default)]
    pub field_errors: Vec<FieldError>,
    /// Set when the file could not be parsed; it is then never patched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    /// Set when the file declares an unsupported `schema_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_error: Option<u32>,
}

/// One item of a save request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemPatchDto {
    /// Parent input of the patch; required in target scope, must match the file in input scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub kind: OverrideItemKind,
    pub id: String,
    pub patch: ItemOverridePatch,
    /// Fallback hint, used only when the raw input item cannot be found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Body of `POST /playlist/overrides`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideSaveRequest {
    pub scope: OverrideScopeDto,
    pub name: String,
    /// Revision of the file the patches were computed against.
    pub base_revision: String,
    #[serde(default)]
    pub patches: Vec<ItemPatchDto>,
}

/// Body of a successful save or discard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideRevisionDto {
    pub revision: String,
}

/// Body of a `409` answer: the file changed underneath the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideConflictDto {
    pub current_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Body of a `422` answer: the patch is well-formed but not valid for the item kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideFieldErrorsDto {
    pub field_errors: Vec<FieldError>,
}

/// Item fields as stored, either of the raw provider record or of the processed target record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemFieldsDto {
    pub name: String,
    pub title: String,
    pub group: String,
    /// `None` when the stored value looks credential-bearing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_small: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    pub chno: u32,
    pub parent_code: String,
    pub audio_track: String,
    pub time_shift: String,
    pub rec: String,
}

/// Which layer an item's fields currently come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideProvenanceDto {
    /// Raw input item, `None` when the input has no stored record for the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ItemFieldsDto>,
    /// Deltas of the parent input's override file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_override: Option<BTreeMap<String, serde_json::Value>>,
    /// Deltas of the target's override file, `None` without a target or without an entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_override: Option<BTreeMap<String, serde_json::Value>>,
    /// Item as the target stored it, `None` without a target or without a stored record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed: Option<ItemFieldsDto>,
    /// `(target, pending)` per target the input feeds: an input edit is pending per affected target.
    #[serde(default)]
    pub pending_per_target: Vec<(String, bool)>,
    /// The override key resolves to more than one item of the input.
    #[serde(default)]
    pub ambiguous: bool,
}

/// Orphan state of one override entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideEntryState {
    /// The item exists in the stored input inventory.
    Active,
    /// The input loaded successfully and does not contain the item any more.
    Orphaned,
    /// The input has no reliable stored inventory, so nothing can be said about the item.
    Unknown,
}

impl OverrideEntryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Orphaned => "orphaned",
            Self::Unknown => "unknown",
        }
    }
}

/// One override entry with its orphan state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideOrphanEntryDto {
    pub scope: OverrideScopeDto,
    /// Name of the override file the entry belongs to (input parent or target name).
    pub file: String,
    /// Parent input the entry was written for; `None` inside an input file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub kind: OverrideItemKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub state: OverrideEntryState,
}

/// An override file whose name matches no configured input or target, e.g. after a rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanedOverrideFileDto {
    pub scope: OverrideScopeDto,
    pub name: String,
    /// Revision needed to discard the file.
    pub revision: String,
}

/// Result of `GET /playlist/overrides/orphans`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideOrphansDto {
    pub input: String,
    #[serde(default)]
    pub entries: Vec<OverrideOrphanEntryDto>,
    #[serde(default)]
    pub orphaned_files: Vec<OrphanedOverrideFileDto>,
}

/// One entry to discard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideDiscardItemDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub kind: OverrideItemKind,
    pub id: String,
}

/// Body of `POST /playlist/overrides/discard`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideDiscardRequest {
    pub scope: OverrideScopeDto,
    pub name: String,
    pub base_revision: String,
    #[serde(default)]
    pub items: Vec<OverrideDiscardItemDto>,
}

/// One EPG channel candidate for the `epg_channel_id` editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpgSearchResultDto {
    pub id: String,
    pub display_name: String,
}
