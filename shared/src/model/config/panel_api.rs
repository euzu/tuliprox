use crate::utils::{
    default_as_true, default_panel_api_alias_pool_max, default_panel_api_alias_pool_min,
    default_panel_api_provision_cooldown_secs, default_panel_api_provision_probe_interval_secs,
    default_panel_api_provision_timeout_secs, deserialize_as_option_string, is_true,
    serialize_vec_flow_map_items,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParamDto {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParametersDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "serialize_vec_flow_map_items")]
    pub account_info: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "serialize_vec_flow_map_items")]
    pub client_info: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "serialize_vec_flow_map_items")]
    pub client_new: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "serialize_vec_flow_map_items")]
    pub client_renew: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", serialize_with = "serialize_vec_flow_map_items" )]
    pub client_adult_content: Vec<PanelApiQueryParamDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PanelApiAliasPoolAuto {
    Auto,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PanelApiAliasPoolSizeValue {
    Auto(PanelApiAliasPoolAuto),
    Number(u16),
}

impl PanelApiAliasPoolSizeValue {
    pub fn as_number(&self) -> Option<u16> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Auto(_) => None,
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto(_))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiAliasPoolSizeDto {
    #[serde(
        default = "default_panel_api_alias_pool_min_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub min: Option<PanelApiAliasPoolSizeValue>,
    #[serde(
        default = "default_panel_api_alias_pool_max_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub max: Option<PanelApiAliasPoolSizeValue>,
}

fn default_panel_api_alias_pool_min_value() -> Option<PanelApiAliasPoolSizeValue> {
    Some(PanelApiAliasPoolSizeValue::Number(
        default_panel_api_alias_pool_min(),
    ))
}

fn default_panel_api_alias_pool_max_value() -> Option<PanelApiAliasPoolSizeValue> {
    Some(PanelApiAliasPoolSizeValue::Number(
        default_panel_api_alias_pool_max(),
    ))
}

impl Default for PanelApiAliasPoolSizeDto {
    fn default() -> Self {
        Self {
            min: default_panel_api_alias_pool_min_value(),
            max: default_panel_api_alias_pool_max_value(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiAliasPoolDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PanelApiAliasPoolSizeDto>,
    #[serde(default)]
    pub remove_expired: bool,
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum PanelApiProvisioningMethod {
    #[default]
    Head,
    Get,
    Post,
}

impl fmt::Display for PanelApiProvisioningMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Head => "HEAD",
            Self::Get => "GET",
            Self::Post => "POST",
        };
        write!(f, "{s}")
    }
}

impl FromStr for PanelApiProvisioningMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "HEAD" => Ok(Self::Head),
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            _ => Err(format!("Unknown provisioning method: {s}")),
        }
    }
}

impl Serialize for PanelApiProvisioningMethod {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PanelApiProvisioningMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiProvisioningDto {
    #[serde(default = "default_panel_api_provision_timeout_secs")]
    pub timeout_sec: u64,
    #[serde(default)]
    pub method: PanelApiProvisioningMethod,
    #[serde(default = "default_panel_api_provision_probe_interval_secs")]
    pub probe_interval_sec: u64,
    #[serde(default = "default_panel_api_provision_cooldown_secs")]
    pub cooldown_sec: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_as_option_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<String>,
}

impl Default for PanelApiProvisioningDto {
    fn default() -> Self {
        Self {
            timeout_sec: default_panel_api_provision_timeout_secs(),
            method: PanelApiProvisioningMethod::default(),
            probe_interval_sec: default_panel_api_provision_probe_interval_secs(),
            cooldown_sec: default_panel_api_provision_cooldown_secs(),
            offset: None,
        }
    }
}

impl PanelApiProvisioningDto {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "PanelApiProvisioningDto::is_default")]
    pub provisioning: PanelApiProvisioningDto,
    #[serde(default)]
    pub query_parameter: PanelApiQueryParametersDto,
    #[serde(
        default,
        deserialize_with = "deserialize_as_option_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub credits: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_pool: Option<PanelApiAliasPoolDto>,
}
