use shared::{error::TuliproxError, utils::Internable};
use std::{fmt, str::FromStr, sync::Arc};

const HOSTED: &str = "hosted";
const PROVIDER: &str = "provider";
const CUSTOM: &str = "custom";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExplorerSourceType {
    Hosted,
    Provider,
    Custom,
}

impl ExplorerSourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExplorerSourceType::Hosted => HOSTED,
            ExplorerSourceType::Provider => PROVIDER,
            ExplorerSourceType::Custom => CUSTOM,
        }
    }
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
        let s = self.as_str();
        write!(f, "{s}")
    }
}

impl Internable for ExplorerSourceType {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}
