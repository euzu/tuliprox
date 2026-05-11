use strum_macros::{Display, EnumIter, EnumString};

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, EnumIter, Display, EnumString,
)]
#[strum(serialize_all = "PascalCase", ascii_case_insensitive)]
#[serde(rename_all = "lowercase")]
pub enum TargetType {
    M3u,
    Xtream,
    Strm,
    HdHomeRun,
}
