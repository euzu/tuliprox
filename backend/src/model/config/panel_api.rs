use crate::model::macros;
use shared::error::{TuliproxError, info_err_res};
use shared::model::{PanelApiAliasPoolDto, PanelApiAliasPoolSizeDto, PanelApiAliasPoolSizeValue, PanelApiConfigDto, PanelApiProvisioningDto, PanelApiQueryParamDto, PanelApiQueryParametersDto};
use std::sync::Arc;
use shared::utils::Internable;

#[derive(Debug, Clone)]
pub struct PanelApiQueryParam {
    pub key: Arc<str>,
    pub value: Arc<str>,
}

macros::from_impl!(PanelApiQueryParam);
impl From<&PanelApiQueryParamDto> for PanelApiQueryParam {
    fn from(dto: &PanelApiQueryParamDto) -> Self {
        Self {
            key: dto.key.clone(),
            value: dto.value.clone(),
        }
    }
}

impl From<&PanelApiQueryParam> for PanelApiQueryParamDto {
    fn from(instance: &PanelApiQueryParam) -> Self {
        Self {
            key: instance.key.clone(),
            value: instance.value.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PanelApiQueryParameters {
    pub account_info: Vec<PanelApiQueryParam>,
    pub client_info: Vec<PanelApiQueryParam>,
    pub client_new: Vec<PanelApiQueryParam>,
    pub client_renew: Vec<PanelApiQueryParam>,
    pub client_adult_content: Vec<PanelApiQueryParam>,
}

macros::from_impl!(PanelApiQueryParameters);
impl From<&PanelApiQueryParametersDto> for PanelApiQueryParameters {
    fn from(dto: &PanelApiQueryParametersDto) -> Self {
        Self {
            account_info: dto.account_info.iter().map(PanelApiQueryParam::from).collect(),
            client_info: dto.client_info.iter().map(PanelApiQueryParam::from).collect(),
            client_new: dto.client_new.iter().map(PanelApiQueryParam::from).collect(),
            client_renew: dto.client_renew.iter().map(PanelApiQueryParam::from).collect(),
            client_adult_content: dto.client_adult_content.iter().map(PanelApiQueryParam::from).collect(),
        }
    }
}

impl From<&PanelApiQueryParameters> for PanelApiQueryParametersDto {
    fn from(instance: &PanelApiQueryParameters) -> Self {
        Self {
            account_info: instance.account_info.iter().map(PanelApiQueryParamDto::from).collect(),
            client_info: instance.client_info.iter().map(PanelApiQueryParamDto::from).collect(),
            client_new: instance.client_new.iter().map(PanelApiQueryParamDto::from).collect(),
            client_renew: instance.client_renew.iter().map(PanelApiQueryParamDto::from).collect(),
            client_adult_content: instance.client_adult_content.iter().map(PanelApiQueryParamDto::from).collect(),
        }
    }
}


impl PanelApiQueryParameters {

    fn validate_type_is_m3u(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
        let typ = params
            .iter()
            .find(|p| p.key.trim().eq_ignore_ascii_case("type"))
            .map(|p| p.value.trim().to_string());
        match typ {
            Some(v) if v.eq_ignore_ascii_case("m3u") => Ok(()),
            Some(v) => info_err_res!("panel_api: unsupported type={v}, only m3u is supported"),
            None => info_err_res!("panel_api: missing required query param 'type=m3u'"),
        }
    }

    fn require_api_key_param(params: &[PanelApiQueryParam], section: &str) -> Result<(), TuliproxError> {
        let api_key = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("api_key"));
        let Some(api_key) = api_key else {
            return info_err_res!("panel_api: {section} must contain query param 'api_key' (use value 'auto')");
        };
        if api_key.value.trim().is_empty() {
            return info_err_res!("panel_api: {section} query param 'api_key' must not be empty (use value 'auto')");
        }
        Ok(())
    }

    fn require_username_password_params_auto(params: &[PanelApiQueryParam], section: &str) -> Result<(), TuliproxError> {
        let username = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("username"));
        let password = params.iter().find(|p| p.key.trim().eq_ignore_ascii_case("password"));
        if username.is_none() || password.is_none() {
            return info_err_res!("panel_api: {section} must contain query params 'username' and 'password' (use value 'auto')");
        }
        if !username.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
            || !password.is_some_and(|p| p.value.trim().eq_ignore_ascii_case("auto"))
        {
            return info_err_res!("panel_api: {section} requires 'username: auto' and 'password: auto' (credentials must not be hardcoded)");
        }
        Ok(())
    }

    fn validate_client_info_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
        Self::require_api_key_param(params, "query_parameter.client_info")?;
        Self::require_username_password_params_auto(params, "query_parameter.client_info")?;
        Ok(())
    }

    fn validate_client_new_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
        Self::require_api_key_param(params, "query_parameter.client_new")?;
        Self::validate_type_is_m3u(params)?;
        if params.iter().any(|p| p.key.trim().eq_ignore_ascii_case("user")) {
            return info_err_res!("panel_api: client_new must not contain query param 'user'");
        }
        Ok(())
    }

    fn validate_client_renew_params(params: &[PanelApiQueryParam]) -> Result<(), TuliproxError> {
        Self::require_api_key_param(params, "query_parameter.client_renew")?;
        Self::validate_type_is_m3u(params)?;
        Self::require_username_password_params_auto(params, "query_parameter.client_renew")?;
        Ok(())
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        Self::validate_client_info_params(&self.client_info)?;
        Self::validate_client_new_params(&self.client_new)?;
        Self::validate_client_renew_params(&self.client_renew)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PanelApiProvisioning {
    pub timeout_sec: u64,
    pub method: shared::model::PanelApiProvisioningMethod,
    pub probe_interval_sec: u64,
    pub cooldown_sec: u64,
    pub offset: Option<Arc<str>>,
}

macros::from_impl!(PanelApiProvisioning);
impl From<&PanelApiProvisioningDto> for PanelApiProvisioning {
    fn from(dto: &PanelApiProvisioningDto) -> Self {
        Self {
            timeout_sec: dto.timeout_sec,
            method: dto.method,
            probe_interval_sec: dto.probe_interval_sec,
            cooldown_sec: dto.cooldown_sec,
            offset: dto.offset.as_ref().map(Internable::intern),
        }
    }
}

impl From<&PanelApiProvisioning> for PanelApiProvisioningDto {
    fn from(instance: &PanelApiProvisioning) -> Self {
        Self {
            timeout_sec: instance.timeout_sec,
            method: instance.method,
            probe_interval_sec: instance.probe_interval_sec,
            cooldown_sec: instance.cooldown_sec,
            offset: instance.offset.as_ref().map(ToString::to_string),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PanelApiAliasPoolSize {
    pub min: Option<PanelApiAliasPoolSizeValue>,
    pub max: Option<PanelApiAliasPoolSizeValue>,
}

macros::from_impl!(PanelApiAliasPoolSize);
impl From<&PanelApiAliasPoolSizeDto> for PanelApiAliasPoolSize {
    fn from(dto: &PanelApiAliasPoolSizeDto) -> Self {
        Self {
            min: dto.min.clone(),
            max: dto.max.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PanelApiAliasPool {
    pub size: Option<PanelApiAliasPoolSize>,
    pub remove_expired: bool,
}

macros::from_impl!(PanelApiAliasPool);
impl From<&PanelApiAliasPoolDto> for PanelApiAliasPool {
    fn from(dto: &PanelApiAliasPoolDto) -> Self {
        Self {
            size: dto.size.as_ref().map(PanelApiAliasPoolSize::from),
            remove_expired: dto.remove_expired,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PanelApiConfig {
    pub enabled: bool,
    pub url: Arc<str>,
    pub api_key: Option<Arc<str>>,
    pub provisioning: PanelApiProvisioning,
    pub query_parameter: PanelApiQueryParameters,
    pub alias_pool: Option<PanelApiAliasPool>,
}

macros::from_impl!(PanelApiConfig);
impl From<&PanelApiConfigDto> for PanelApiConfig {
    fn from(dto: &PanelApiConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            url: dto.url.clone().intern(),
            api_key: dto.api_key.as_ref().map(Internable::intern),
            provisioning: PanelApiProvisioning::from(&dto.provisioning),
            query_parameter: PanelApiQueryParameters::from(&dto.query_parameter),
            alias_pool: dto.alias_pool.as_ref().map(PanelApiAliasPool::from),
        }
    }
}

impl From<&PanelApiConfig> for PanelApiConfigDto {
    fn from(instance: &PanelApiConfig) -> Self {
        Self {
            enabled: instance.enabled,
            url: instance.url.to_string(),
            api_key: instance.api_key.as_ref().map(Arc::clone),
            provisioning: PanelApiProvisioningDto::from(&instance.provisioning),
            query_parameter: PanelApiQueryParametersDto::from(&instance.query_parameter),
            credits: None,
            alias_pool: instance.alias_pool.as_ref().map(|p| PanelApiAliasPoolDto {
                size: p.size.as_ref().map(|s| PanelApiAliasPoolSizeDto {
                    min: s.min.clone(),
                    max: s.max.clone(),
                }),
                remove_expired: p.remove_expired,
            }),
        }
    }
}

impl PanelApiConfig {

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if !self.enabled {
            return Ok(());
        }
        if self.url.trim().is_empty() {
            return info_err_res!("panel_api: url is missing");
        }
        if self.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
            return info_err_res!("panel_api: api_key is missing");
        }
        if self.query_parameter.client_info.is_empty()
            || self.query_parameter.client_new.is_empty()
            || self.query_parameter.client_renew.is_empty()
        {
            return info_err_res!("panel_api: query_parameter.client_info/client_new/client_renew must be configured");
        }
        self.query_parameter.prepare()
    }
}