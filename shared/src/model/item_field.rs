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
        })
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
