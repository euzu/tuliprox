use std::str::FromStr;
use shared::error::{info_err, TuliproxError, TuliproxErrorKind};

pub enum ViewType {
    Dashboard,
    Users,
}

impl FromStr for ViewType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "dashboard" => Ok(ViewType::Dashboard),
            "users" => Ok(ViewType::Users),
            _ => Err(info_err!(format!("Unknown view type: {s}"))),
        }
    }
}