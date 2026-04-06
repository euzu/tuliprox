use shared::{error::TuliproxError, utils::Internable};
use std::{fmt::Display, str::FromStr, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UserlistPage {
    List,
    Edit,
}

impl FromStr for UserlistPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "list" => Ok(UserlistPage::List),
            "edit" => Ok(UserlistPage::Edit),
            _ => Err(TuliproxError::Config(format!("Unknown page type: {s}"))),
        }
    }
}

impl Display for UserlistPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::List => "list",
                Self::Edit => "edit",
            }
        )
    }
}

impl Internable for UserlistPage {
    fn intern(self) -> Arc<str> {
        match self {
            Self::List => "list",
            Self::Edit => "edit",
        }
        .intern()
    }
}
