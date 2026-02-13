use crate::app::components::{Card, CollapsePanel, TextButton};
use crate::{
    config_field, edit_field_number_u16, edit_field_number_u8, edit_field_text,
    generate_form_reducer, html_if,
};
use shared::model::HdHomeRunDeviceConfigDto;
use yew::prelude::*;
use yew_i18n::use_translation;

generate_form_reducer!(
    state: HdHomeRunDeviceConfigFormState { form: Box<HdHomeRunDeviceConfigDto> },
    action_name: HdHomeRunDeviceConfigFormAction,
    fields {
        FriendlyName => friendly_name: String,
        Manufacturer => manufacturer: String,
        ModelName => model_name: String,
        ModelNumber => model_number: String,
        FirmwareName => firmware_name: String,
        FirmwareVersion => firmware_version: String,
        DeviceType => device_type: String,
        DeviceUdn => device_udn: String,
        Name => name: String,
        Port => port: u16,
        TunerCount => tuner_count: u8,
    }
);

#[derive(Properties, PartialEq)]
pub struct HdHomerunDeviceViewProps {
    pub device_id: usize, // this is a transient Id to identify the dto in parent.
    pub device: HdHomeRunDeviceConfigDto,
    pub on_form_change: Callback<(usize, bool, HdHomeRunDeviceConfigDto)>,
    pub on_remove: Callback<usize>,
    #[prop_or(false)]
    pub edit_mode: bool,
}

#[function_component]
pub fn HdHomerunDeviceView(props: &HdHomerunDeviceViewProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<HdHomeRunDeviceConfigFormState> =
        use_reducer(|| HdHomeRunDeviceConfigFormState {
            form: Box::new(props.device.clone()),
            modified: false,
        });

    {
        let on_form_change = props.on_form_change.clone();
        let device_id = props.device_id;
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit((device_id, *modified, (*state.form).clone()));
        });
    }

    let handle_remove_device = {
        let device_id = props.device_id;
        let onremove = props.on_remove.clone();
        Callback::from(move |_| {
            onremove.emit(device_id);
        })
    };

    let render_device = |device_state: &UseReducerHandle<HdHomeRunDeviceConfigFormState>| -> Html {
        html! {
            <Card class = "tp__config-view__card" >
                <div class = "tp__config-view__header">
                    <h1> { translate.t("LABEL.DEVICE") } </h1>
                    {html_if!(props.edit_mode, {
                       <TextButton name="remove_device" class="secondary" title={translate.t("LABEL.DELETE")} icon="Delete" onclick={handle_remove_device} />
                    })}
                </div>
                if props.edit_mode {
                    {edit_field_text!(device_state, translate.t("LABEL.NAME"), name, HdHomeRunDeviceConfigFormAction::Name)}
                    {edit_field_number_u16!(device_state, translate.t("LABEL.PORT"), port, HdHomeRunDeviceConfigFormAction::Port)}
                    {edit_field_number_u8!(device_state, translate.t("LABEL.TUNER_COUNT"), tuner_count, HdHomeRunDeviceConfigFormAction::TunerCount)}
                    {edit_field_text!(device_state, translate.t("LABEL.DEVICE_UDN"), device_udn, HdHomeRunDeviceConfigFormAction::DeviceUdn)}
                    <CollapsePanel expanded={false} class="tp__hdhomerun__device-extended-fields"
                            title={translate.t("LABEL.EXTENDED_ATTRIBUTES")}>
                    <>
                    {edit_field_text!(device_state, translate.t("LABEL.FRIENDLY_NAME"), friendly_name, HdHomeRunDeviceConfigFormAction::FriendlyName)}
                    {edit_field_text!(device_state, translate.t("LABEL.MANUFACTURER"), manufacturer, HdHomeRunDeviceConfigFormAction::Manufacturer)}
                    {edit_field_text!(device_state, translate.t("LABEL.MODEL_NAME"), model_name, HdHomeRunDeviceConfigFormAction::ModelName)}
                    {edit_field_text!(device_state, translate.t("LABEL.MODEL_NUMBER"), model_number, HdHomeRunDeviceConfigFormAction::ModelNumber)}
                    {edit_field_text!(device_state, translate.t("LABEL.FIRMWARE_NAME"), firmware_name, HdHomeRunDeviceConfigFormAction::FirmwareName)}
                    {edit_field_text!(device_state, translate.t("LABEL.FIRMWARE_VERSION"), firmware_version, HdHomeRunDeviceConfigFormAction::FirmwareVersion)}
                    {edit_field_text!(device_state, translate.t("LABEL.DEVICE_TYPE"), device_type, HdHomeRunDeviceConfigFormAction::DeviceType)}
                    </>
                    </CollapsePanel>
                 } else {
                    {config_field!(&device_state.form, translate.t("LABEL.NAME"), name)}
                    {config_field!(&device_state.form, translate.t("LABEL.PORT"), port)}
                    {config_field!(&device_state.form, translate.t("LABEL.TUNER_COUNT"), tuner_count)}
                    {config_field!(&device_state.form, translate.t("LABEL.DEVICE_UDN"), device_udn)}
                    <CollapsePanel expanded={false} class="tp__hdhomerun__device-extended-fields"
                            title={translate.t("LABEL.EXTENDED_ATTRIBUTES")}>
                    <>
                    {config_field!(&device_state.form, translate.t("LABEL.FRIENDLY_NAME"), friendly_name)}
                    {config_field!(&device_state.form, translate.t("LABEL.MANUFACTURER"), manufacturer)}
                    {config_field!(&device_state.form, translate.t("LABEL.MODEL_NAME"), model_name)}
                    {config_field!(&device_state.form, translate.t("LABEL.MODEL_NUMBER"), model_number)}
                    {config_field!(&device_state.form, translate.t("LABEL.FIRMWARE_NAME"), firmware_name)}
                    {config_field!(&device_state.form, translate.t("LABEL.FIRMWARE_VERSION"), firmware_version)}
                    {config_field!(&device_state.form, translate.t("LABEL.DEVICE_TYPE"), device_type)}
                    </>
                    </CollapsePanel>
                }
            </Card>
        }
    };

    render_device(&form_state)
}
