use shared::utils::Internable;
use std::sync::Arc;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    strum_macros::Display,
    strum_macros::EnumString,
    strum_macros::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum ExplorerSourceType {
    Hosted,
    Provider,
    Custom,
}

impl ExplorerSourceType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Provider => "provider",
            Self::Custom => "custom",
        }
    }
}

impl Internable for ExplorerSourceType {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}
