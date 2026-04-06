use shared::error::TuliproxError;
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExplorerSourceType {
    Hosted,
    Provider,
    Custom,
}

impl FromStr for ExplorerSourceType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "hosted" => Ok(ExplorerSourceType::Hosted),
            "provider" => Ok(ExplorerSourceType::Provider),
            "custom" => Ok(ExplorerSourceType::Custom),
            _ => Err(TuliproxError::Config(format!("Unknown explorer source type: {s}"))),
        }
    }
}

impl fmt::Display for ExplorerSourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ExplorerSourceType::Hosted => "hosted",
            ExplorerSourceType::Provider => "provider",
            ExplorerSourceType::Custom => "custom",
        };
        write!(f, "{s}")
    }
}
