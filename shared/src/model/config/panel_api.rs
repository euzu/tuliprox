use crate::error::TuliproxError;
use crate::info_err_res;
use crate::utils::{
    default_as_true, default_panel_api_alias_pool_max, default_panel_api_alias_pool_min,
    default_panel_api_provision_cooldown_secs, default_panel_api_provision_probe_interval_secs,
    default_panel_api_provision_timeout_secs, deserialize_as_option_string, is_true,
    serialize_vec_flow_map_items, arc_str_serde, arc_str_option_serde, is_blank_optional_arc_str
};
use log::warn;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParamDto {
    #[serde(with = "arc_str_serde")]
    pub key: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub value: Arc<str>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParametersDto {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_vec_flow_map_items"
    )]
    pub account_info: Vec<PanelApiQueryParamDto>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_vec_flow_map_items"
    )]
    pub client_info: Vec<PanelApiQueryParamDto>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_vec_flow_map_items"
    )]
    pub client_new: Vec<PanelApiQueryParamDto>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_vec_flow_map_items"
    )]
    pub client_renew: Vec<PanelApiQueryParamDto>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_vec_flow_map_items"
    )]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub url: String,
    #[serde(with = "arc_str_option_serde", skip_serializing_if = "is_blank_optional_arc_str")]
    pub api_key: Option<Arc<str>>,
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

impl Default for PanelApiConfigDto {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            api_key: None,
            provisioning: PanelApiProvisioningDto::default(),
            query_parameter: PanelApiQueryParametersDto::default(),
            credits: None,
            alias_pool: None,
        }
    }
}

impl PanelApiConfigDto {
    pub fn prepare(&mut self, input_name: &Arc<str>) -> Result<(), TuliproxError> {
        if self.enabled {
            if let Some(alias_pool) = self.alias_pool.as_mut() {
                let size = alias_pool
                    .size
                    .get_or_insert_with(PanelApiAliasPoolSizeDto::default);
               // Capture original state before applying defaults
                let min_was_none = size.min.is_none();
                if size.min.is_none() {
                    size.min = Some(PanelApiAliasPoolSizeValue::Number(1));
                }
                if size.max.is_none() {
                    size.max = Some(PanelApiAliasPoolSizeValue::Number(1));
                }
                let min = size
                    .min
                    .as_ref()
                    .and_then(PanelApiAliasPoolSizeValue::as_number);
                let max = size
                    .max
                    .as_ref()
                    .and_then(PanelApiAliasPoolSizeValue::as_number);
                if let (Some(min), Some(max)) = (min, max) {
                    if min > max {
                        return info_err_res!("panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max");
                    }
                }

                let max_auto = size
                    .max
                    .as_ref()
                    .is_some_and(PanelApiAliasPoolSizeValue::is_auto);
                if max_auto && min_was_none {
                    warn!("panel_api.alias_pool.size.max is set to auto without min for input {input_name}");
                }
            }

            if self.provisioning.probe_interval_sec == 0 {
                return info_err_res!("panel_api.provisioning.probe_interval_sec must be greater than 0");
            }
        }
        Ok(())
    }
}