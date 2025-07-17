use std::fmt;
use std::str::FromStr;
use shared::error::{info_err, TuliproxError, TuliproxErrorKind};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewType {
    Dashboard,
    Stats,
    Users,
    Playlists,
}

impl FromStr for ViewType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "dashboard" => Ok(ViewType::Dashboard),
            "stats" => Ok(ViewType::Stats),
            "users" => Ok(ViewType::Users),
            "playlists" => Ok(ViewType::Playlists),
            _ => Err(info_err!(format!("Unknown view type: {s}"))),
        }
    }
}

impl fmt::Display for ViewType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ViewType::Dashboard => "dashboard",
            ViewType::Stats => "stats",
            ViewType::Users => "users",
            ViewType::Playlists => "playlists",
        };
        write!(f, "{s}")
    }
}