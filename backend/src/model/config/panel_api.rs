use crate::model::macros;
use shared::error::{TuliproxError, info_err_res};
use shared::model::{PanelApiAliasPoolDto, PanelApiAliasPoolSizeDto, PanelApiAliasPoolSizeValue, PanelApiConfigDto, PanelApiProvisioningDto, PanelApiQueryParamDto, PanelApiQueryParametersDto};

#[derive(Debug, Clone)]
pub struct PanelApiQueryParam {
    pub key: String,
    pub value: String,
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
    pub url: String,
    pub api_key: Option<String>,
    pub query_parameter: PanelApiQueryParameters,
    pub alias_pool: Option<PanelApiAliasPool>,
}

macros::from_impl!(PanelApiConfig);
impl From<&PanelApiConfigDto> for PanelApiConfig {
    fn from(dto: &PanelApiConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            url: dto.url.clone(),
            api_key: dto.api_key.clone(),
            query_parameter: PanelApiQueryParameters::from(&dto.query_parameter),
            alias_pool: dto.alias_pool.as_ref().map(PanelApiAliasPool::from),
        }
    }
}

impl From<&PanelApiConfig> for PanelApiConfigDto {
    fn from(instance: &PanelApiConfig) -> Self {
        Self {
            enabled: instance.enabled,
            url: instance.url.clone(),
            api_key: instance.api_key.clone(),
            provisioning: PanelApiProvisioningDto::default(),
            query_parameter: PanelApiQueryParametersDto::from(&instance.query_parameter),

            credits: None,
            alias_pool: None,
        }
    }
}

impl PanelApiConfig {

    // fn prepare_panel_api(&mut self) -> Result<(), TuliproxError> {
    //     if let Some(panel) = self.panel_api.as_mut() {
    //         if panel.enabled {
    //             if let Some(alias_pool) = panel.alias_pool.as_mut() {
    //                 let size = alias_pool
    //                     .size
    //                     .get_or_insert_with(PanelApiAliasPoolSizeDto::default);
    //                 if size.min.is_none() {
    //                     size.min = Some(PanelApiAliasPoolSizeValue::Number(1));
    //                 }
    //                 if size.max.is_none() {
    //                     size.max = Some(PanelApiAliasPoolSizeValue::Number(1));
    //                 }
    //                 let min = size
    //                     .min
    //                     .as_ref()
    //                     .and_then(PanelApiAliasPoolSizeValue::as_number);
    //                 let max = size
    //                     .max
    //                     .as_ref()
    //                     .and_then(PanelApiAliasPoolSizeValue::as_number);
    //                 if let (Some(min), Some(max)) = (min, max) {
    //                     if min > max {
    //                         return info_err_res!("panel_api.alias_pool.size.min must be <= panel_api.alias_pool.size.max");
    //                     }
    //                 }
    //
    //                 let max_auto = size
    //                     .max
    //                     .as_ref()
    //                     .is_some_and(PanelApiAliasPoolSizeValue::is_auto);
    //                 if max_auto && size.min.is_none() {
    //                     warn!(
    //                         "panel_api.alias_pool.size.max is set to auto without min for input {}",
    //                         self.name
    //                     );
    //                 }
    //             }
    //
    //             if panel.provisioning.probe_interval_sec == 0 {
    //                 return info_err_res!(
    //                     "panel_api.provisioning.probe_interval_sec must be greater than 0"
    //                 );
    //             }
    //         }
    //     }
    //     Ok(())
    // }

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