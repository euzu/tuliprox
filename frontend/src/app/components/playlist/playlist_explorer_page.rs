use std::fmt::Display;
use std::str::FromStr;
use shared::error::{TuliproxError, info_err_res};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaylistExplorerPage {
    SourceSelector,
}

impl FromStr for PlaylistExplorerPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "source-selector" => Ok(PlaylistExplorerPage::SourceSelector),
            _ => info_err_res!("Unknown page type: {s}"),
        }
    }
}

impl Display for PlaylistExplorerPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            Self::SourceSelector => "source-selector",
        })
    }
}