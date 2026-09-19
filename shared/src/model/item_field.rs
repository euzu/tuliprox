use serde::{Deserialize, Deserializer};
use std::{fmt::Display, str::FromStr};
use strum_macros::{AsRefStr, EnumIter, EnumString};

#[derive(Debug, Copy, Clone, serde::Serialize, Eq, PartialEq, EnumIter, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
#[serde(rename_all = "lowercase")]
pub enum ItemField {
    Group,
    Name,
    Title,
    Genre,
    Url,
    Input,
    Type,
    Caption,
    EpgId,
    Chno,
    Quality,
}

impl<'de> Deserialize<'de> for ItemField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ItemField::from_str(&s).map_err(serde::de::Error::custom)
    }
}

impl Display for ItemField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Group => "Group",
            Self::Name => "Name",
            Self::Title => "Title",
            Self::Genre => "Genre",
            Self::Url => "Url",
            Self::Input => "Input",
            Self::Type => "Type",
            Self::Caption => "Caption",
            Self::EpgId => "EpgId",
            Self::Chno => "Chno",
            Self::Quality => "Quality",
        })
    }
}

/// Every named field reachable on a playlist item or its header.
///
/// This is deliberately *wider* than [`ItemField`]. `ItemField` is the
/// config-facing vocabulary — what a user may name in a filter, sort or mapper
/// rule — and widening it would widen the config surface. The accessor's domain
/// is larger: `logo_small`, `audio_track` and friends are addressable by name
/// (the M3U resource endpoint does exactly that) without being valid config
/// fields.
///
/// Not every field exists on every type: `Id` is a header field while
/// `ProviderId` is an M3U one, and each accessor returns `None` for fields its
/// type does not carry. That mirrors the previous `&str` behaviour exactly.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum HeaderField {
    Id,
    ProviderId,
    Title,
    Name,
    Logo,
    LogoSmall,
    ParentCode,
    AudioTrack,
    TimeShift,
    Rec,
    Url,
    Group,
    Caption,
    Input,
    Type,
    EpgChannelId,
    Chno,
    Genre,
}

impl HeaderField {
    /// Case-insensitive lookup by field name.
    ///
    /// Cold path: only the `&str` compatibility shims reach here. Callers that
    /// already hold a `HeaderField` — the mapper, sort and counter paths — match
    /// on the discriminant instead of comparing strings.
    ///
    /// `epg_id` is accepted as an alias for `epg_channel_id`, as before.
    pub fn parse(name: &str) -> Option<Self> {
        const TABLE: &[(&str, HeaderField)] = &[
            ("id", HeaderField::Id),
            ("provider_id", HeaderField::ProviderId),
            ("title", HeaderField::Title),
            ("name", HeaderField::Name),
            ("logo", HeaderField::Logo),
            ("logo_small", HeaderField::LogoSmall),
            ("parent_code", HeaderField::ParentCode),
            ("audio_track", HeaderField::AudioTrack),
            ("time_shift", HeaderField::TimeShift),
            ("rec", HeaderField::Rec),
            ("url", HeaderField::Url),
            ("group", HeaderField::Group),
            ("caption", HeaderField::Caption),
            ("input", HeaderField::Input),
            ("type", HeaderField::Type),
            ("epg_channel_id", HeaderField::EpgChannelId),
            ("epg_id", HeaderField::EpgChannelId),
            ("chno", HeaderField::Chno),
            ("genre", HeaderField::Genre),
        ];
        TABLE.iter().find(|(key, _)| name.eq_ignore_ascii_case(key)).map(|(_, field)| *field)
    }

    /// The canonical name, i.e. the inverse of [`Self::parse`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::ProviderId => "provider_id",
            Self::Title => "title",
            Self::Name => "name",
            Self::Logo => "logo",
            Self::LogoSmall => "logo_small",
            Self::ParentCode => "parent_code",
            Self::AudioTrack => "audio_track",
            Self::TimeShift => "time_shift",
            Self::Rec => "rec",
            Self::Url => "url",
            Self::Group => "group",
            Self::Caption => "caption",
            Self::Input => "input",
            Self::Type => "type",
            Self::EpgChannelId => "epg_channel_id",
            Self::Chno => "chno",
            Self::Genre => "genre",
        }
    }
}

impl Display for HeaderField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

impl AsRef<str> for HeaderField {
    fn as_ref(&self) -> &str { self.as_str() }
}

impl ItemField {
    /// The header field this config field reads.
    ///
    /// `Quality` is derived from the caption rather than stored on the header,
    /// so it maps to nothing and is resolved by `ValueProvider` instead.
    pub const fn header_field(self) -> Option<HeaderField> {
        match self {
            Self::Group => Some(HeaderField::Group),
            Self::Name => Some(HeaderField::Name),
            Self::Title => Some(HeaderField::Title),
            Self::Genre => Some(HeaderField::Genre),
            Self::Url => Some(HeaderField::Url),
            Self::Input => Some(HeaderField::Input),
            Self::Type => Some(HeaderField::Type),
            Self::Caption => Some(HeaderField::Caption),
            Self::EpgId => Some(HeaderField::EpgChannelId),
            Self::Chno => Some(HeaderField::Chno),
            Self::Quality => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_field_from_str_keeps_legacy_case_insensitive_parsing() {
        assert_eq!(ItemField::from_str("Group").expect("legacy field casing parses"), ItemField::Group);
        assert_eq!(ItemField::from_str("TITLE").expect("upper case field parses"), ItemField::Title);
    }

    #[test]
    fn item_field_deserialize_uses_case_insensitive_strum_parser() {
        assert_eq!(
            serde_json::from_str::<ItemField>("\"Group\"").expect("legacy field casing parses"),
            ItemField::Group
        );
        assert_eq!(serde_json::from_str::<ItemField>("\"TITLE\"").expect("upper case field parses"), ItemField::Title);
    }

    #[test]
    fn item_field_serialize_keeps_lowercase_config_shape() {
        assert_eq!(serde_json::to_string(&ItemField::Group).expect("item field serializes"), "\"group\"");
    }
}
