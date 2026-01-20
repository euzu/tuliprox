use crate::app::components::config::config_page::{ConfigForm, LABEL_PANEL_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::input::Input;
use crate::app::components::select::Select;
use crate::app::components::{
    Card, Chip, DropDownOption, DropDownSelection, IconButton, ToggleSwitch,
};
use crate::app::context::ConfigContext;
use crate::html_if;
use shared::model::{
    ConfigInputDto, PanelApiAliasPoolAuto, PanelApiAliasPoolSizeDto, PanelApiAliasPoolSizeValue,
    PanelApiConfigDto, PanelApiProvisioningMethod, PanelApiQueryParamDto, SourcesConfigDto,
};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::utils::Internable;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_API_KEY: &str = "LABEL.API_KEY";
const LABEL_STATUS_READY: &str = "LABEL.PANEL_STATUS_READY";
const LABEL_STATUS_INVALID: &str = "LABEL.PANEL_STATUS_INVALID";
const LABEL_STATUS_DISABLED: &str = "LABEL.PANEL_STATUS_DISABLED";
const LABEL_PANEL_ADULT_CONTENT: &str = "LABEL.PANEL_ADULT_CONTENT";
const LABEL_PANEL_CREDITS: &str = "LABEL.PANEL_CREDITS";
const LABEL_VALIDATION: &str = "LABEL.VALIDATION";
const HINT_PANEL_INFO: &str = "HINT.CONFIG.PANEL.INFO";
const HINT_PANEL_ENABLE: &str = "HINT.CONFIG.PANEL.ENABLE";
const LABEL_PANEL_PROVISIONING: &str = "LABEL.PANEL_PROVISIONING";
const LABEL_PANEL_PROVISION_TIMEOUT: &str = "LABEL.PANEL_PROVISION_TIMEOUT_SEC";
const LABEL_PANEL_PROBE_INTERVAL: &str = "LABEL.PANEL_PROBE_INTERVAL_SEC";
const LABEL_PANEL_PROVISION_COOLDOWN: &str = "LABEL.PANEL_PROVISION_COOLDOWN_SEC";
const LABEL_PANEL_PROVISION_METHOD: &str = "LABEL.PANEL_PROVISION_METHOD";
const LABEL_PANEL_PROVISION_OFFSET: &str = "LABEL.PANEL_PROVISION_OFFSET";
const LABEL_PANEL_ALIAS_POOL: &str = "LABEL.PANEL_ALIAS_POOL";
const LABEL_PANEL_ALIAS_POOL_MIN: &str = "LABEL.PANEL_ALIAS_POOL_MIN";
const LABEL_PANEL_ALIAS_POOL_MAX: &str = "LABEL.PANEL_ALIAS_POOL_MAX";
const LABEL_PANEL_ALIAS_POOL_REMOVE_EXPIRED: &str = "LABEL.PANEL_ALIAS_POOL_REMOVE_EXPIRED";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PanelSection {
    AccountInfo,
    Info,
    New,
    Renew,
    AdultContent,
}

impl PanelSection {
    fn label_key(&self) -> &'static str {
        match self {
            Self::AccountInfo => "LABEL.PANEL_ACCOUNT_INFO",
            Self::Info => "LABEL.PANEL_CLIENT_INFO",
            Self::New => "LABEL.PANEL_CLIENT_NEW",
            Self::Renew => "LABEL.PANEL_CLIENT_RENEW",
            Self::AdultContent => LABEL_PANEL_ADULT_CONTENT,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PanelConfigFormState {
    form: SourcesConfigDto,
    modified: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum PanelConfigFormAction {
    SetAll(SourcesConfigDto),
    SetEnabled {
        input_idx: usize,
        enabled: bool,
    },
    SetPanelUrl {
        input_idx: usize,
        url: String,
    },
    SetApiKey {
        input_idx: usize,
        api_key: String,
    },
    SetProvisioningTimeout {
        input_idx: usize,
        timeout_sec: u64,
    },
    SetProvisioningProbeInterval {
        input_idx: usize,
        probe_interval_sec: u64,
    },
    SetProvisioningCooldown {
        input_idx: usize,
        cooldown_sec: u64,
    },
    SetProvisioningMethod {
        input_idx: usize,
        method: PanelApiProvisioningMethod,
    },
    SetProvisioningOffset {
        input_idx: usize,
        offset: String,
    },
    SetAliasPoolMin {
        input_idx: usize,
        value: Option<PanelApiAliasPoolSizeValue>,
    },
    SetAliasPoolMax {
        input_idx: usize,
        value: Option<PanelApiAliasPoolSizeValue>,
    },
    SetAliasPoolRemoveExpired {
        input_idx: usize,
        remove_expired: bool,
    },
    AddParam {
        input_idx: usize,
        section: PanelSection,
    },
    RemoveParam {
        input_idx: usize,
        section: PanelSection,
        param_idx: usize,
    },
    SetParamKey {
        input_idx: usize,
        section: PanelSection,
        param_idx: usize,
        key: String,
    },
    SetParamValue {
        input_idx: usize,
        section: PanelSection,
        param_idx: usize,
        value: String,
    },
    EnsureRequired {
        input_idx: usize,
        section: PanelSection,
    },
}

fn params_mut(
    panel: &mut PanelApiConfigDto,
    section: PanelSection,
) -> &mut Vec<PanelApiQueryParamDto> {
    match section {
        PanelSection::AccountInfo => &mut panel.query_parameter.account_info,
        PanelSection::Info => &mut panel.query_parameter.client_info,
        PanelSection::New => &mut panel.query_parameter.client_new,
        PanelSection::Renew => &mut panel.query_parameter.client_renew,
        PanelSection::AdultContent => &mut panel.query_parameter.client_adult_content,
    }
}

fn has_param(params: &[PanelApiQueryParamDto], key: &str) -> bool {
    params
        .iter()
        .any(|p| p.key.trim().eq_ignore_ascii_case(key))
}

fn get_param_value(params: &[PanelApiQueryParamDto], key: &str) -> Option<String> {
    params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case(key))
        .map(|p| p.value.trim().to_string())
}

fn alias_pool_size_to_string(value: Option<&PanelApiAliasPoolSizeValue>) -> String {
    match value {
        Some(PanelApiAliasPoolSizeValue::Auto(_)) => "auto".to_string(),
        Some(PanelApiAliasPoolSizeValue::Number(num)) => num.to_string(),
        None => String::new(),
    }
}

fn parse_alias_pool_size(value: &str) -> Option<PanelApiAliasPoolSizeValue> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    // this is for the search
    if "auto".starts_with(&trimmed.to_ascii_lowercase()) {
        return Some(PanelApiAliasPoolSizeValue::Auto(
            PanelApiAliasPoolAuto::Auto,
        ));
    }
    trimmed
        .parse::<u16>()
        .ok()
        .map(PanelApiAliasPoolSizeValue::Number)
}

fn validate_panel(panel: Option<&PanelApiConfigDto>) -> Vec<String> {
    let mut errors = vec![];
    let Some(panel) = panel else {
        return errors;
    };
    if !panel.enabled {
        return errors;
    }

    if panel.url.trim().is_empty() {
        errors.push("Missing panel url".to_string());
    }
    if panel.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        errors.push("Missing api_key".to_string());
    }

    let sections = [
        (
            PanelSection::AccountInfo,
            &panel.query_parameter.account_info,
            false,
        ),
        (PanelSection::Info, &panel.query_parameter.client_info, true),
        (PanelSection::New, &panel.query_parameter.client_new, true),
        (
            PanelSection::Renew,
            &panel.query_parameter.client_renew,
            true,
        ),
        (
            PanelSection::AdultContent,
            &panel.query_parameter.client_adult_content,
            false,
        ),
    ];
    for (section, params, required) in sections {
        if params.is_empty() {
            if required {
                errors.push(match section {
                    PanelSection::AccountInfo => "account_info: empty".to_string(),
                    PanelSection::Info => "client_info: empty".to_string(),
                    PanelSection::New => "client_new: empty".to_string(),
                    PanelSection::Renew => "client_renew: empty".to_string(),
                    PanelSection::AdultContent => "client_adult_content: empty".to_string(),
                });
            }
            continue;
        }
        if !has_param(params, "api_key") {
            errors.push(match section {
                PanelSection::AccountInfo => "account_info: missing api_key param".to_string(),
                PanelSection::Info => "client_info: missing api_key param".to_string(),
                PanelSection::New => "client_new: missing api_key param".to_string(),
                PanelSection::Renew => "client_renew: missing api_key param".to_string(),
                PanelSection::AdultContent => {
                    "client_adult_content: missing api_key param".to_string()
                }
            });
        }
        match section {
            PanelSection::AccountInfo => {
                if has_param(params, "username") || has_param(params, "password") {
                    if get_param_value(params, "username").as_deref() != Some("auto") {
                        errors.push("account_info: username must be auto".to_string());
                    }
                    if get_param_value(params, "password").as_deref() != Some("auto") {
                        errors.push("account_info: password must be auto".to_string());
                    }
                }
            }
            PanelSection::New => {
                if has_param(params, "user") {
                    errors.push("client_new: must not include user".to_string());
                }
                if get_param_value(params, "type").as_deref() != Some("m3u") {
                    errors.push("client_new: type must be m3u".to_string());
                }
            }
            PanelSection::Renew => {
                if get_param_value(params, "type").as_deref() != Some("m3u") {
                    errors.push("client_renew: type must be m3u".to_string());
                }
                if get_param_value(params, "username").as_deref() != Some("auto") {
                    errors.push("client_renew: username must be auto".to_string());
                }
                if get_param_value(params, "password").as_deref() != Some("auto") {
                    errors.push("client_renew: password must be auto".to_string());
                }
            }
            PanelSection::Info => {
                if get_param_value(params, "username").as_deref() != Some("auto") {
                    errors.push("client_info: username must be auto".to_string());
                }
                if get_param_value(params, "password").as_deref() != Some("auto") {
                    errors.push("client_info: password must be auto".to_string());
                }
            }
            PanelSection::AdultContent => {
                if has_param(params, "username") || has_param(params, "password") {
                    if get_param_value(params, "username").as_deref() != Some("auto") {
                        errors.push("client_adult_content: username must be auto".to_string());
                    }
                    if get_param_value(params, "password").as_deref() != Some("auto") {
                        errors.push("client_adult_content: password must be auto".to_string());
                    }
                }
            }
        }
    }

    if let Some(size) = panel.alias_pool.as_ref().and_then(|p| p.size.as_ref()) {
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
                errors.push("alias_pool.size: min must be <= max".to_string());
            }
        }
        if let Some(min) = min {
            if min == 0 {
                errors.push("alias_pool.size: min must be > 0".to_string());
            }
        }
        if let Some(max) = max {
            if max == 0 {
                errors.push("alias_pool.size: max must be > 0".to_string());
            }
        }
    }
    if panel.provisioning.probe_interval_sec == 0 {
        errors.push("provisioning: probe_interval_sec must be > 0".to_string());
    }
    if let Some(offset) = panel.provisioning.offset.as_deref() {
        if !offset.trim().is_empty() && parse_offset_secs(offset).is_none() {
            errors.push("provisioning: offset must be a number with optional suffix s/m/h/d (e.g. 30m, 12h)".to_string());
        }
    }
    errors
}

fn parse_offset_secs(value: &str) -> Option<u64> {
    let raw = value.trim();
    if raw.is_empty() {
        return Some(0);
    }
    let lower = raw.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let last = *bytes.last()?;
    let (num_part, multiplier) = match last {
        b's' => (&lower[..lower.len().saturating_sub(1)], 1_u64),
        b'm' => (&lower[..lower.len().saturating_sub(1)], 60_u64),
        b'h' => (&lower[..lower.len().saturating_sub(1)], 60_u64 * 60),
        b'd' => (&lower[..lower.len().saturating_sub(1)], 60_u64 * 60 * 24),
        b'0'..=b'9' => (lower.as_str(), 1_u64),
        _ => return None,
    };
    let num_part = num_part.trim();
    if num_part.is_empty() {
        return None;
    }
    let value: u64 = num_part.parse().ok()?;
    value.checked_mul(multiplier)
}

fn ensure_required_params(params: &mut Vec<PanelApiQueryParamDto>, section: PanelSection) {
    let ensure = |params: &mut Vec<PanelApiQueryParamDto>, key: &str, value: &str| {
        if !has_param(params, key) {
            params.push(PanelApiQueryParamDto {
                key: key.intern(),
                value: value.intern(),
            });
        }
    };
    ensure(params, "api_key", "auto");
    match section {
        PanelSection::AccountInfo => {
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
        PanelSection::New => {
            params.retain(|p| !p.key.trim().eq_ignore_ascii_case("user"));
            ensure(params, "type", "m3u");
        }
        PanelSection::Renew => {
            ensure(params, "type", "m3u");
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
        PanelSection::Info => {
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
        PanelSection::AdultContent => {
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
    }
}

fn with_input_mut(
    form: &mut SourcesConfigDto,
    input_idx: usize,
    f: impl FnOnce(&mut ConfigInputDto),
) {
    if let Some(input) = form.inputs.get_mut(input_idx) {
        f(input);
    }
}

impl Reducible for PanelConfigFormState {
    type Action = PanelConfigFormAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        match action {
            PanelConfigFormAction::SetAll(form) => Self {
                form,
                modified: false,
            }
            .into(),
            PanelConfigFormAction::SetEnabled {
                input_idx,
                enabled,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if enabled {
                        let panel = input
                            .panel_api
                            .get_or_insert_with(PanelApiConfigDto::default);
                        panel.enabled = true;
                    } else if let Some(panel) = input.panel_api.as_mut() {
                        panel.enabled = false;
                    }
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetPanelUrl {
                input_idx,
                url,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.url = url;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetApiKey {
                input_idx,
                api_key,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.api_key = if api_key.trim().is_empty() {
                        None
                    } else {
                        Some(api_key.intern())
                    };
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetProvisioningTimeout {
                input_idx,
                timeout_sec,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.provisioning.timeout_sec = timeout_sec;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetProvisioningProbeInterval {
                input_idx,
                probe_interval_sec,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.provisioning.probe_interval_sec = probe_interval_sec;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetProvisioningCooldown {
                input_idx,
                cooldown_sec,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.provisioning.cooldown_sec = cooldown_sec;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetProvisioningMethod {
                input_idx,
                method,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    panel.provisioning.method = method;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetProvisioningOffset {
                input_idx,
                offset,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    let normalized = offset.trim().to_string();
                    panel.provisioning.offset = if normalized.is_empty() {
                        None
                    } else {
                        Some(normalized)
                    };
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetAliasPoolMin {
                input_idx,
                value,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    let alias_pool = panel.alias_pool.get_or_insert_with(Default::default);
                    let size = alias_pool.size.get_or_insert_with(Default::default);
                    size.min = value;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetAliasPoolMax {
                input_idx,
                value,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    let alias_pool = panel.alias_pool.get_or_insert_with(Default::default);
                    let size = alias_pool.size.get_or_insert_with(Default::default);
                    size.max = value;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetAliasPoolRemoveExpired {
                input_idx,
                remove_expired,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    let alias_pool = panel.alias_pool.get_or_insert_with(Default::default);
                    alias_pool.remove_expired = remove_expired;
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::AddParam {
                input_idx,
                section,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    params_mut(panel, section).push(PanelApiQueryParamDto::default());
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::RemoveParam {
                input_idx,
                section,
                param_idx,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if param_idx < params.len() {
                            params.remove(param_idx);
                        }
                    }
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetParamKey {
                input_idx,
                section,
                param_idx,
                key,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if let Some(p) = params.get_mut(param_idx) {
                            p.key = key.intern();
                        }
                    }
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::SetParamValue {
                input_idx,
                section,
                param_idx,
                value,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if let Some(p) = params.get_mut(param_idx) {
                            p.value = value.intern();
                        }
                    }
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
            PanelConfigFormAction::EnsureRequired {
                input_idx,
                section,
            } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input
                        .panel_api
                        .get_or_insert_with(PanelApiConfigDto::default);
                    ensure_required_params(params_mut(panel, section), section);
                });
                Self {
                    form,
                    modified: true,
                }
                .into()
            }
        }
    }
}

fn render_param_editor(
    form_state: &UseReducerHandle<PanelConfigFormState>,
    edit_mode: bool,
    input_idx: usize,
    section: PanelSection,
    section_title: String,
    params: &[PanelApiQueryParamDto],
) -> Html {
    let add_required = {
        let form_state = form_state.clone();
        Callback::from(move |(name, _): (String, MouseEvent)| {
            if name == "required" {
                form_state.dispatch(PanelConfigFormAction::EnsureRequired {
                    input_idx,
                    section,
                });
            }
        })
    };

    let add_param = {
        let form_state = form_state.clone();
        Callback::from(move |(name, _): (String, MouseEvent)| {
            if name == "add" {
                form_state.dispatch(PanelConfigFormAction::AddParam {
                    input_idx,
                    section,
                });
            }
        })
    };

    html! {
        <div class="tp__panel-api-config-view__section">
            <div class="tp__panel-api-config-view__section-header">
                <h2>{ section_title }</h2>
                { html_if!(edit_mode, {
                    <div class="tp__panel-api-config-view__section-actions">
                        <IconButton name="required" icon="Accept" class="secondary" onclick={add_required}/>
                        <IconButton name="add" icon="Add" class="primary" onclick={add_param}/>
                    </div>
                })}
            </div>
            <div class="tp__panel-api-config-view__params">
            {
                if params.is_empty() {
                    html!{ <div class="tp__panel-api-config-view__params-empty">{ "—" }</div> }
                } else {
                    html!{ for params.iter().enumerate().map(|(param_idx, p)| {
                        let on_key = {
                            let form_state = form_state.clone();
                            Callback::from(move |value: String| {
                                form_state.dispatch(PanelConfigFormAction::SetParamKey { input_idx, section, param_idx, key: value });
                            })
                        };
                        let on_val = {
                            let form_state = form_state.clone();
                            Callback::from(move |value: String| {
                                form_state.dispatch(PanelConfigFormAction::SetParamValue { input_idx, section, param_idx, value });
                            })
                        };
                        let on_remove = {
                            let form_state = form_state.clone();
                            Callback::from(move |(name, _): (String, MouseEvent)| {
                                if name == "rm" {
                                    form_state.dispatch(PanelConfigFormAction::RemoveParam { input_idx, section, param_idx });
                                }
                            })
                        };
                        html!{
                            <div class={classes!(
                                "tp__panel-api-config-view__param-row",
                                if edit_mode { None } else { Some("tp__panel-api-config-view__param-row--view") }
                            )}>
                                {
                                    if edit_mode {
                                        html!{
                                            <>
                                                <Input name="key" label={Option::<String>::None} value={p.key.to_string()} placeholder={Some("key".to_string())} on_change={Some(on_key)} />
                                                <Input name="value" label={Option::<String>::None} value={p.value.to_string()} placeholder={Some("value".to_string())} on_change={Some(on_val)} />
                                                <IconButton name="rm" icon="Delete" class="tp__panel-api-config-view__param-remove" onclick={on_remove}/>
                                            </>
                                        }
                                    } else {
                                        html!{
                                            <div class="tp__panel-api-config-view__param-view">
                                                <span class="k">{ &p.key }</span>
                                                <span class="v">{ &p.value }</span>
                                            </div>
                                        }
                                    }
                                }
                            </div>
                        }
                    }) }
                }
            }
            </div>
        </div>
    }
}

#[function_component]
pub fn PanelConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<PanelConfigFormState> = use_reducer(|| PanelConfigFormState {
        form: SourcesConfigDto::default(),
        modified: false,
    });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::Panel(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let sources_cfg = config_ctx.config.as_ref().map(|c| c.sources.clone());
        use_effect_with(
            (sources_cfg, config_view_ctx.edit_mode.clone()),
            move |(sources_cfg, _mode)| {
                if let Some(src) = sources_cfg {
                    form_state.dispatch(PanelConfigFormAction::SetAll((*src).clone()));
                } else {
                    form_state.dispatch(PanelConfigFormAction::SetAll(SourcesConfigDto::default()));
                }
                || ()
            },
        );
    }

    let render_input_card = |input_idx: usize, input: &ConfigInputDto| -> Html {
        let panel_enabled = input.panel_api.as_ref().is_some_and(|panel| panel.enabled);
        let errors = validate_panel(input.panel_api.as_ref());
        let has_errors = !errors.is_empty();
        let status_chip = if panel_enabled {
            if has_errors {
                html! { <Chip label={translate.t(LABEL_STATUS_INVALID)} class={Some("inactive".to_string())}/> }
            } else {
                html! { <Chip label={translate.t(LABEL_STATUS_READY)} class={Some("active".to_string())}/> }
            }
        } else {
            html! { <Chip label={translate.t(LABEL_STATUS_DISABLED)} class={Option::<String>::None}/> }
        };

        let on_toggle = {
            let form_state = form_state.clone();
            Callback::from(move |enabled: bool| {
                form_state.dispatch(PanelConfigFormAction::SetEnabled {
                    input_idx,
                    enabled,
                });
            })
        };

        let on_url = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetPanelUrl {
                    input_idx,
                    url: value,
                });
            })
        };

        let on_api_key = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetApiKey {
                    input_idx,
                    api_key: value,
                });
            })
        };
        let on_provision_timeout = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                let timeout_sec = value.trim().parse::<u64>().unwrap_or(0);
                form_state.dispatch(PanelConfigFormAction::SetProvisioningTimeout {
                    input_idx,
                    timeout_sec,
                });
            })
        };
        let on_probe_interval = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                let probe_interval_sec = value.trim().parse::<u64>().unwrap_or(0);
                form_state.dispatch(PanelConfigFormAction::SetProvisioningProbeInterval {
                    input_idx,
                    probe_interval_sec,
                });
            })
        };
        let on_provision_cooldown = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                let cooldown_sec = value.trim().parse::<u64>().unwrap_or(0);
                form_state.dispatch(PanelConfigFormAction::SetProvisioningCooldown {
                    input_idx,
                    cooldown_sec,
                });
            })
        };
        let on_method_select = {
            let form_state = form_state.clone();
            Callback::from(move |(_name, selection): (String, DropDownSelection)| {
                if let DropDownSelection::Single(option) = selection {
                    if let Ok(method) = option.parse::<PanelApiProvisioningMethod>() {
                        form_state.dispatch(PanelConfigFormAction::SetProvisioningMethod {
                            input_idx,
                            method,
                        });
                    }
                }
            })
        };
        let on_provision_offset = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetProvisioningOffset {
                    input_idx,
                    offset: value,
                });
            })
        };
        let on_alias_pool_min = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetAliasPoolMin {
                    input_idx,
                    value: parse_alias_pool_size(&value),
                });
            })
        };
        let on_alias_pool_max = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetAliasPoolMax {
                    input_idx,
                    value: parse_alias_pool_size(&value),
                });
            })
        };
        let on_alias_pool_remove_expired = {
            let form_state = form_state.clone();
            Callback::from(move |value: bool| {
                form_state.dispatch(PanelConfigFormAction::SetAliasPoolRemoveExpired {
                    input_idx,
                    remove_expired: value,
                });
            })
        };

        let panel = input.panel_api.as_ref();
        let url_val = panel.map(|p| p.url.clone()).unwrap_or_default();
        let api_key_val = panel.and_then(|p| p.api_key.clone()).unwrap_or_default();
        let provisioning_timeout_val = panel
            .map(|p| p.provisioning.timeout_sec.to_string())
            .unwrap_or_default();
        let provisioning_probe_interval_val = panel
            .map(|p| p.provisioning.probe_interval_sec.to_string())
            .unwrap_or_default();
        let provisioning_cooldown_val = panel
            .map(|p| p.provisioning.cooldown_sec.to_string())
            .unwrap_or_default();
        let provisioning_offset_val = panel
            .and_then(|p| p.provisioning.offset.as_ref())
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        let provisioning_method = panel.map(|p| p.provisioning.method).unwrap_or_default();
        let alias_pool = panel.and_then(|p| p.alias_pool.as_ref());
        let alias_pool_size_default = PanelApiAliasPoolSizeDto::default();
        let alias_pool_size = alias_pool
            .and_then(|p| p.size.as_ref())
            .unwrap_or(&alias_pool_size_default);
        let alias_pool_min = alias_pool_size.min.as_ref();
        let alias_pool_max = alias_pool_size.max.as_ref();
        let alias_pool_min_val = alias_pool_size_to_string(alias_pool_min);
        let alias_pool_max_val = alias_pool_size_to_string(alias_pool_max);
        let alias_pool_remove_expired = alias_pool.is_some_and(|p| p.remove_expired);
        let client_info = panel
            .map(|p| p.query_parameter.client_info.as_slice())
            .unwrap_or(&[]);
        let client_new = panel
            .map(|p| p.query_parameter.client_new.as_slice())
            .unwrap_or(&[]);
        let client_renew = panel
            .map(|p| p.query_parameter.client_renew.as_slice())
            .unwrap_or(&[]);
        let account_info = panel
            .map(|p| p.query_parameter.account_info.as_slice())
            .unwrap_or(&[]);
        let adult_content = panel
            .map(|p| p.query_parameter.client_adult_content.as_slice())
            .unwrap_or(&[]);
        let credits_value = panel
            .and_then(|p| p.credits.as_ref())
            .map(|credits| credits.trim().to_string())
            .filter(|credits| !credits.is_empty())
            .unwrap_or_else(|| "—".to_string());
        let credits_label = format!("{}: {}", translate.t(LABEL_PANEL_CREDITS), credits_value);
        let type_label = input.input_type.to_string().to_uppercase();
        let provisioning_method_label = provisioning_method.to_string();
        let provisioning_summary = format!(
            "{} / {}s / {}s / {}s",
            provisioning_method_label,
            provisioning_timeout_val,
            provisioning_probe_interval_val,
            provisioning_cooldown_val
        );
        let alias_pool_min_label = if alias_pool_min_val.is_empty() {
            "—".to_string()
        } else {
            alias_pool_min_val.clone()
        };
        let alias_pool_max_label = if alias_pool_max_val.is_empty() {
            "—".to_string()
        } else {
            alias_pool_max_val.clone()
        };
        let alias_pool_remove_label = if alias_pool_remove_expired {
            "true".to_string()
        } else {
            "false".to_string()
        };
        let method_options = Rc::new(vec![
            DropDownOption::new(
                "HEAD",
                html! {<span>{ "HEAD" }</span>},
                provisioning_method == PanelApiProvisioningMethod::Head,
            ),
            DropDownOption::new(
                "GET",
                html! {<span>{ "GET" }</span>},
                provisioning_method == PanelApiProvisioningMethod::Get,
            ),
            DropDownOption::new(
                "POST",
                html! {<span>{ "POST" }</span>},
                provisioning_method == PanelApiProvisioningMethod::Post,
            ),
        ]);

        html! {
            <Card class="tp__config-view__card tp__panel-api-config-view__input-card">
                <div class="tp__panel-api-config-view__input-header">
                    <div class="tp__panel-api-config-view__input-title">
                        <h1>{ &input.name }</h1>
                        <div class="tp__panel-api-config-view__input-badges">
                            <Chip label={type_label} class={Option::<String>::None}/>
                            <Chip label={credits_label} class={Some("tp__panel-api-config-view__credits-chip".to_string())}/>
                        </div>
                    </div>
                    <div class="tp__panel-api-config-view__input-status">
                        { status_chip }
                        { html_if!(*config_view_ctx.edit_mode, {
                            <div class="tp__panel-api-config-view__toggle">
                                <span class="lbl">{ translate.t(LABEL_ENABLED) }</span>
                                <ToggleSwitch value={panel_enabled} readonly={false} on_change={on_toggle} />
                            </div>
                        })}
                    </div>
                </div>

                {
                    if !panel_enabled {
                        html! { <div class="tp__panel-api-config-view__disabled-hint">{ translate.t(HINT_PANEL_ENABLE) }</div> }
                    } else if *config_view_ctx.edit_mode {
                        html! {
                            <>
                                <Input name="panel_url" label={Some(translate.t(LABEL_URL))} value={url_val} on_change={Some(on_url)} placeholder={Some("https://panel.example.tld/api.php".to_string())}/>
                                <Input name="panel_api_key" label={Some(translate.t(LABEL_API_KEY))} value={api_key_val.to_string()} hidden={true} on_change={Some(on_api_key)} placeholder={Some("...".to_string())}/>
                                <div class="tp__panel-api-config-view__section">
                                    <div class="tp__panel-api-config-view__section-header">
                                        <h2>{ translate.t(LABEL_PANEL_PROVISIONING) }</h2>
                                    </div>
                                    <div class="tp__panel-api-config-view__params">
                                        <div class="tp__panel-api-config-view__param-row">
                                            <Input name="panel_provision_timeout"
                                                label={Some(translate.t(LABEL_PANEL_PROVISION_TIMEOUT))}
                                                value={provisioning_timeout_val.clone()}
                                                on_change={Some(on_provision_timeout)}
                                                placeholder={Some("60".to_string())}/>
                                            <Input name="panel_probe_interval"
                                                label={Some(translate.t(LABEL_PANEL_PROBE_INTERVAL))}
                                                value={provisioning_probe_interval_val.clone()}
                                                on_change={Some(on_probe_interval)}
                                                placeholder={Some("5".to_string())}/>
                                            <Input name="panel_provision_cooldown"
                                                label={Some(translate.t(LABEL_PANEL_PROVISION_COOLDOWN))}
                                                value={provisioning_cooldown_val.clone()}
                                                on_change={Some(on_provision_cooldown)}
                                                placeholder={Some("0".to_string())}/>
                                            <Input name="panel_provision_offset"
                                                label={Some(translate.t(LABEL_PANEL_PROVISION_OFFSET))}
                                                value={provisioning_offset_val.clone()}
                                                on_change={Some(on_provision_offset)}
                                                placeholder={Some("30m".to_string())}/>
                                            <div class="tp__input">
                                                <label>{ translate.t(LABEL_PANEL_PROVISION_METHOD) }</label>
                                                <Select
                                                    name="panel_provision_method"
                                                    options={method_options.clone()}
                                                    on_select={on_method_select.clone()}
                                                    />
                                            </div>
                                        </div>
                                    </div>
                                </div>
                                <div class="tp__panel-api-config-view__section">
                                    <div class="tp__panel-api-config-view__section-header">
                                        <h2>{ translate.t(LABEL_PANEL_ALIAS_POOL) }</h2>
                                    </div>
                                    <div class="tp__panel-api-config-view__params">
                                        <div class="tp__panel-api-config-view__param-row">
                                            <Input name="panel_alias_pool_min"
                                                label={Some(translate.t(LABEL_PANEL_ALIAS_POOL_MIN))}
                                                value={alias_pool_min_val.clone()}
                                                on_change={Some(on_alias_pool_min)}
                                                placeholder={Some("auto|<number>".to_string())}/>
                                                <Input name="panel_alias_pool_max"
                                                    label={Some(translate.t(LABEL_PANEL_ALIAS_POOL_MAX))}
                                                    value={alias_pool_max_val.clone()}
                                                    on_change={Some(on_alias_pool_max)}
                                                    placeholder={Some("auto|<number>".to_string())}/>
                                                <div class="tp__input">
                                                    <label>{ translate.t(LABEL_PANEL_ALIAS_POOL_REMOVE_EXPIRED) }</label>
                                                    <div class="tp__panel-api-config-view__toggle">
                                                        <span class="lbl">{ translate.t(LABEL_ENABLED) }</span>
                                                        <ToggleSwitch value={alias_pool_remove_expired} readonly={false} on_change={on_alias_pool_remove_expired} />
                                                    </div>
                                                </div>
                                            </div>
                                        </div>
                                    </div>
                                { render_param_editor(&form_state, true, input_idx, PanelSection::AccountInfo, translate.t(PanelSection::AccountInfo.label_key()), account_info) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::Info, translate.t(PanelSection::Info.label_key()), client_info) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::New, translate.t(PanelSection::New.label_key()), client_new) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::Renew, translate.t(PanelSection::Renew.label_key()), client_renew) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::AdultContent, translate.t(PanelSection::AdultContent.label_key()), adult_content) }
                                { html_if!(has_errors, {
                                    <div class="tp__panel-api-config-view__errors">
                                        <h2>{ translate.t(LABEL_VALIDATION) }</h2>
                                        <ul>
                                            { for errors.iter().map(|e| html!{ <li>{ e }</li> }) }
                                        </ul>
                                    </div>
                                })}
                            </>
                        }
                    } else {
                        html!{
                            <>
                                <div class="tp__panel-api-config-view__summary">
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_URL) }</label>
                                        <span class="tp__form-field__value">{ if url_val.is_empty() { "—".to_string() } else { url_val } }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_API_KEY) }</label>
                                        <span class="tp__form-field__value">{ if api_key_val.is_empty() { "—".to_string() } else { "••••••••".to_string() } }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_PANEL_PROVISIONING) }</label>
                                        <span class="tp__form-field__value">{ provisioning_summary }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_PANEL_ALIAS_POOL_MIN) }</label>
                                        <span class="tp__form-field__value">{ alias_pool_min_label }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_PANEL_ALIAS_POOL_MAX) }</label>
                                        <span class="tp__form-field__value">{ alias_pool_max_label }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_PANEL_ALIAS_POOL_REMOVE_EXPIRED) }</label>
                                        <span class="tp__form-field__value">{ alias_pool_remove_label }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::AccountInfo.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", account_info.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::Info.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_info.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::New.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_new.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::Renew.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_renew.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::AdultContent.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", adult_content.len()) }</span>
                                    </div>
                                </div>
                                { html_if!(has_errors, {
                                    <div class="tp__panel-api-config-view__errors">
                                        <h2>{ translate.t(LABEL_VALIDATION) }</h2>
                                        <ul>
                                            { for errors.iter().map(|e| html!{ <li>{ e }</li> }) }
                                        </ul>
                                    </div>
                                })}
                            </>
                        }
                    }
                }
            </Card>
        }
    };

    let inputs = form_state
        .form
        .inputs
        .iter()
        .enumerate()
        .collect::<Vec<_>>();

    html! {
        <div class="tp__panel-api-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{ translate.t(LABEL_PANEL_CONFIG) }</div>
            <div class="tp__panel-api-config-view__header tp__config-view-page__header">
                <Card class="tp__config-view__card tp__panel-api-config-view__info-card">
                    <h1>{ translate.t(LABEL_PANEL_CONFIG) }</h1>
                    <div class="tp__panel-api-config-view__info-text">
                        { translate.t(HINT_PANEL_INFO) }
                    </div>
                </Card>
            </div>
            <div class="tp__panel-api-config-view__body tp__config-view-page__body">
                { for inputs.into_iter().map(|(input_idx, inp)| render_input_card(input_idx, inp)) }
            </div>
        </div>
    }
}
