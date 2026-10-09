use super::{stalker::prepare_stalker_config, InputType};
use crate::{
    check_input_connections, check_input_credentials,
    defaults::{default_as_true, is_true, is_zero_i16, is_zero_u16},
    error::TuliproxError,
    model::StalkerInputConfigDto,
    utils::{arc_str_serde, deserialize_timestamp, is_blank_optional_string, Internable},
};
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputAliasDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub url: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalker: Option<StalkerInputConfigDto>,
}

impl ConfigInputAliasDto {
    pub fn prepare(&mut self, index: u16, input_type: &InputType) -> Result<u16, TuliproxError> {
        self.id = index + 1;
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigInput("name for input is mandatory".to_string()));
        }
        self.url = self.url.trim().to_string();
        if self.url.is_empty() {
            return Err(TuliproxError::ConfigInput(format!("url for input is mandatory (input: {})", self.name)));
        }
        check_input_credentials!(self, input_type, true, true);
        check_input_connections!(self, input_type, true);
        prepare_stalker_config(
            &self.name,
            input_type,
            &mut self.stalker,
            true,
            self.username.as_deref(),
            self.password.as_deref(),
        )?;

        Ok(self.id)
    }
}
