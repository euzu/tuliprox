use super::{common::CommonInputForm, ConfigInputFormState};
use crate::{
    app::components::{
        number_input::NumberInput, select::Select, selection_parse_first, DropDownOption, DropDownSelection,
    },
    config_field, config_field_child, config_field_optional, edit_field_text_option, generate_form_reducer,
    i18n::{use_translation, YewI18n},
};
use shared::model::{
    stalker::StalkerActionSizeCapDto, StalkerAuthMode, StalkerDeviceProfileDto, StalkerEndpointPreference,
    StalkerInputConfigDto, StalkerMagPreset,
};
use strum::IntoEnumIterator;
use yew::{component, html, use_memo, Callback, Html, Properties, UseReducerHandle, UseStateHandle};

generate_form_reducer!(
    state: StalkerDeviceFormState { form: StalkerDeviceProfileDto },
    action_name: StalkerDeviceFormAction,
    fields {
        MacAddress => mac_address: Option<String>,
        DeviceProfile => device_profile: Option<String>,
        SerialNumber => serial_number: Option<String>,
        DeviceId => device_id: Option<String>,
        DeviceId2 => device_id2: Option<String>,
        Signature => signature: Option<String>,
        Timezone => timezone: Option<String>,
        Locale => locale: Option<String>,
        UserAgent => user_agent: Option<String>,
        XUserAgent => x_user_agent: Option<String>,
    }
);

pub(super) fn empty_device_form_state() -> StalkerDeviceFormState {
    StalkerDeviceFormState { form: StalkerDeviceProfileDto::default(), modified: false }
}

fn parsed_size_cap(value: Option<i64>) -> Option<u32> { value.and_then(|value| u32::try_from(value).ok()) }

fn size_cap_field(
    state: &UseStateHandle<StalkerInputConfigDto>,
    label: String,
    name: &'static str,
    field_id: &'static str,
    value: u32,
    update: fn(&mut StalkerActionSizeCapDto, u32),
) -> Html {
    let state = state.clone();
    html! {
        <div class="tp__form-field tp__form-field__number">
            <NumberInput label={label} name={name} field_id={field_id} value={i64::from(value)}
                on_change={Callback::from(move |value| {
                    let Some(value) = parsed_size_cap(value) else { return; };
                    let mut config = (*state).clone();
                    update(config.size_caps.get_or_insert_with(Default::default), value);
                    state.set(config);
                })} />
        </div>
    }
}

pub(super) fn stalker_options_fields(
    state: &UseStateHandle<StalkerInputConfigDto>,
    allow_write: bool,
    translate: &YewI18n,
) -> Html {
    let config: &StalkerInputConfigDto = state;
    let caps = state.size_caps.clone().unwrap_or_default();

    if !allow_write {
        return html! {
            <>
                { config_field!(caps, translate.t("LABEL.STALKER_CREATE_LINK_KB"), create_link_kb) }
                { config_field!(caps, translate.t("LABEL.STALKER_ORDERED_LIST_MB"), ordered_list_mb) }
                { config_field!(caps, translate.t("LABEL.STALKER_GET_EPG_MB"), get_epg_mb) }
                { config_field_optional!(config, translate.t("LABEL.STALKER_CATALOG_PAGE_LIMIT"), catalog_max_pages) }
            </>
        };
    }

    let page_limit_state = state.clone();
    html! {
        <>
            { size_cap_field(state, translate.t("LABEL.STALKER_CREATE_LINK_KB"), "stalker_create_link_kb", "STALKER_ACTION_SIZE_CAP.CREATE_LINK_KB", caps.create_link_kb, |caps, value| caps.create_link_kb = value) }
            { size_cap_field(state, translate.t("LABEL.STALKER_ORDERED_LIST_MB"), "stalker_ordered_list_mb", "STALKER_ACTION_SIZE_CAP.ORDERED_LIST_MB", caps.ordered_list_mb, |caps, value| caps.ordered_list_mb = value) }
            { size_cap_field(state, translate.t("LABEL.STALKER_GET_EPG_MB"), "stalker_get_epg_mb", "STALKER_ACTION_SIZE_CAP.GET_EPG_MB", caps.get_epg_mb, |caps, value| caps.get_epg_mb = value) }
            <div class="tp__form-field tp__form-field__number">
                <NumberInput label={translate.t("LABEL.STALKER_CATALOG_PAGE_LIMIT")}
                    name="stalker_catalog_page_limit" value={state.catalog_max_pages.map(i64::from)}
                    on_change={Callback::from(move |value| {
                        let mut config = (*page_limit_state).clone();
                        config.catalog_max_pages = parsed_size_cap(value).filter(|value| *value > 0);
                        page_limit_state.set(config);
                    })} />
            </div>
        </>
    }
}

#[derive(Properties, Clone)]
pub(super) struct StalkerInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub config: UseStateHandle<StalkerInputConfigDto>,
    pub allow_write: bool,
}

impl PartialEq for StalkerInputFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn StalkerInputForm(props: &StalkerInputFormProps) -> Html {
    let translate = use_translation();
    let config = props.config.clone();
    let config_value: &StalkerInputConfigDto = &config;
    let auth_options = use_memo(config.auth_mode, |selected| enum_options::<StalkerAuthMode>(*selected));
    let preset_options = use_memo(config.mag_preset, |selected| enum_options::<StalkerMagPreset>(*selected));
    let endpoint_options =
        use_memo(config.endpoint_preference, |selected| enum_options::<StalkerEndpointPreference>(*selected));

    let extra = if props.allow_write {
        html! {
            <>
                { select_field(&config, translate.t("LABEL.STALKER_AUTH_MODE"), "stalker_auth_mode", "STALKER_INPUT_CONFIG.AUTH_MODE", auth_options, |config, value| config.auth_mode = value) }
                { select_field(&config, translate.t("LABEL.STALKER_MAG_PRESET"), "stalker_mag_preset", "STALKER_INPUT_CONFIG.MAG_PRESET", preset_options, |config, value| config.mag_preset = value) }
                { select_field(&config, translate.t("LABEL.STALKER_ENDPOINT_PREFERENCE"), "stalker_endpoint_preference", "STALKER_INPUT_CONFIG.ENDPOINT_PREFERENCE", endpoint_options, |config, value| config.endpoint_preference = value) }
            </>
        }
    } else {
        html! {
            <>
                { config_field!(config_value, translate.t("LABEL.STALKER_AUTH_MODE"), auth_mode) }
                { config_field!(config_value, translate.t("LABEL.STALKER_MAG_PRESET"), mag_preset) }
                { config_field!(config_value, translate.t("LABEL.STALKER_ENDPOINT_PREFERENCE"), endpoint_preference) }
            </>
        }
    };

    html! {
        <CommonInputForm state={props.state.clone()} allow_write={props.allow_write}
            credentials={matches!(
                config.auth_mode,
                StalkerAuthMode::CredentialsOnly | StalkerAuthMode::MacPlusCredentials
            )}
            connection={true} cache_duration={true} extra={extra} />
    }
}

fn enum_options<T>(selected: T) -> Vec<DropDownOption>
where
    T: IntoEnumIterator + ToString + Copy + PartialEq,
{
    T::iter()
        .map(|value| DropDownOption {
            id: value.to_string(),
            label: html! { value.to_string() },
            selected: value == selected,
        })
        .collect()
}

fn select_field<T>(
    state: &UseStateHandle<StalkerInputConfigDto>,
    label: String,
    name: &'static str,
    field_id: &'static str,
    options: std::rc::Rc<Vec<DropDownOption>>,
    update: fn(&mut StalkerInputConfigDto, T),
) -> Html
where
    T: std::str::FromStr + 'static,
{
    let state = state.clone();
    config_field_child!(label, field_id, {
        html! { <Select name={name} multi_select={false}
        on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
            if let Some(value) = selection_parse_first(&selections) {
                let mut config = (*state).clone();
                update(&mut config, value);
                state.set(config);
            }
        })} options={options} /> }
    })
}

#[derive(Properties, Clone)]
pub(super) struct StalkerDeviceInputFormProps {
    pub state: UseReducerHandle<StalkerDeviceFormState>,
    pub allow_write: bool,
}

impl PartialEq for StalkerDeviceInputFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn StalkerDeviceInputForm(props: &StalkerDeviceInputFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    if !props.allow_write {
        return html! {
            <crate::app::components::Card class="tp__config-view__card">
                <div class="tp__config-view__cols-2">
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_MAC_ADDRESS"), mac_address) }
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_TIMEZONE"), timezone) }
                </div>
                <div class="tp__config-view__cols-2">
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_LOCALE"), locale) }
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_USER_AGENT"), user_agent) }
                </div>
                <div class="tp__config-view__cols-2">
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_X_USER_AGENT"), x_user_agent) }
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_DEVICE_PROFILE"), device_profile) }
                </div>
                <div class="tp__config-view__cols-2">
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_SERIAL_NUMBER"), serial_number) }
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_DEVICE_ID"), device_id) }
                </div>
                <div class="tp__config-view__cols-2">
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_DEVICE_ID2"), device_id2) }
                    { config_field_optional!(state.form, translate.t("LABEL.STALKER_SIGNATURE"), signature) }
                </div>
            </crate::app::components::Card>
        };
    }
    html! {
        <crate::app::components::Card class="tp__config-view__card">
            <div class="tp__config-view__cols-2">
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_MAC_ADDRESS"), mac_address, StalkerDeviceFormAction::MacAddress) }
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_TIMEZONE"), timezone, StalkerDeviceFormAction::Timezone) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_LOCALE"), locale, StalkerDeviceFormAction::Locale) }
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_USER_AGENT"), user_agent, StalkerDeviceFormAction::UserAgent) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_X_USER_AGENT"), x_user_agent, StalkerDeviceFormAction::XUserAgent) }
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_DEVICE_PROFILE"), device_profile, StalkerDeviceFormAction::DeviceProfile) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_SERIAL_NUMBER"), serial_number, StalkerDeviceFormAction::SerialNumber) }
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_DEVICE_ID"), device_id, StalkerDeviceFormAction::DeviceId) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_DEVICE_ID2"), device_id2, StalkerDeviceFormAction::DeviceId2) }
                { edit_field_text_option!(state, translate.t("LABEL.STALKER_SIGNATURE"), signature, StalkerDeviceFormAction::Signature) }
            </div>
        </crate::app::components::Card>
    }
}

#[cfg(test)]
mod tests {
    use super::parsed_size_cap;

    #[test]
    fn size_caps_ignore_blank_negative_and_out_of_range_values() {
        assert_eq!(parsed_size_cap(None), None);
        assert_eq!(parsed_size_cap(Some(-1)), None);
        assert_eq!(parsed_size_cap(Some(i64::from(u32::MAX) + 1)), None);
        assert_eq!(parsed_size_cap(Some(64)), Some(64));
    }
}
