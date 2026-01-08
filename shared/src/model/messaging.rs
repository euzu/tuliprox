use std::fmt;
use std::str::FromStr;
use crate::info_err_res;
use crate::error::{TuliproxError};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum MsgKind {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "stats")]
    Stats,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "watch")]
    Watch,
}
impl fmt::Display for MsgKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MsgKind::Info => "Info",
            MsgKind::Stats => "Stats",
            MsgKind::Error => "Error",
            MsgKind::Watch => "Watch",
        };
        write!(f, "{s}")
    }
}

impl FromStr for MsgKind {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq_ignore_ascii_case("info") {
            Ok(Self::Info)
        } else if s.eq_ignore_ascii_case("stats") {
            Ok(Self::Stats)
        } else if s.eq_ignore_ascii_case("error") {
            Ok(Self::Error)
        } else if s.eq_ignore_ascii_case("watch") {
            Ok(Self::Watch)
        } else {
            info_err_res!("Unknown MsgKind: {}", s)
        }
    }
}