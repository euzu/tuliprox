use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum_macros::Display, strum_macros::EnumString)]
pub enum ConfigType {
    Config,
    ApiProxy,
    Mapping,
    Template,
    Sources,
}
