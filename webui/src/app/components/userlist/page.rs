use std::fmt::Display;
use std::str::FromStr;
use shared::error::{TuliproxError,TuliproxErrorKind};
use shared::info_err;

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
            _ => Err(info_err!(format!("Unknown page type: {s}"))),
        }
    }
}

impl Display for UserlistPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            Self::List => "list",
            Self::Edit => "edit",
        })
    }
}