use crate::app::components::config::config_page::{ConfigForm, LABEL_HDHOMERUN_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::config::hdhomerun_device_view::HdHomerunDeviceView;
use crate::app::components::{Card, NoContent, TextButton};
use crate::app::context::ConfigContext;
use crate::{config_field_bool, edit_field_bool, generate_form_reducer, html_if};
use log::error;
use shared::model::{HdHomeRunConfigDto, HdHomeRunDeviceConfigDto};
use yew::prelude::*;
use yew_i18n::use_translation;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_DEVICE_AUTH: &str = "LABEL.DEVICE_AUTH";
const LABEL_DEVICES: &str = "LABEL.DEVICES";
const LABEL_SSDP_DISCOVERY: &str = "LABEL.SSDP_DISCOVERY";
const LABEL_PROPRIETARY_DISCOVERY: &str = "LABEL.PROPRIETARY_DISCOVERY";

generate_form_reducer!(
    state: HdHomeRunConfigFormState { form: HdHomeRunConfigDto },
    action_name: HdHomeRunConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Auth => auth: bool,
        SsdpDiscovery => ssdp_discovery: bool,
        ProprietaryDiscovery => proprietary_discovery: bool,
        Devices => devices: Vec<HdHomeRunDeviceConfigDto>,
    }
);

#[function_component]
pub fn HdHomerunConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<HdHomeRunConfigFormState> =
        use_reducer(|| HdHomeRunConfigFormState {
            form: HdHomeRunConfigDto::default(),
            modified: false,
        });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::HdHomerun(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let hdhr_config = config_ctx
            .config
            .as_ref()
            .and_then(|c| c.config.hdhomerun.clone());

        use_effect_with(
            (hdhr_config, config_view_ctx.edit_mode.clone()),
            move |(hdhr_cfg, _mode)| {
                if let Some(hdhr) = hdhr_cfg {
                    form_state.dispatch(HdHomeRunConfigFormAction::SetAll((*hdhr).clone()));
                } else {
                    form_state.dispatch(HdHomeRunConfigFormAction::SetAll(
                        HdHomeRunConfigDto::default(),
                    ));
                }
                || ()
            },
        );
    }

    let handle_device_change = {
        let form_state = form_state.clone();
        Callback::from(move |(device_idx, _device_modified, device)| {
            let mut devices = form_state.form.devices.clone();
            if let Some(slot) = devices.get_mut(device_idx) {
                *slot = device;
            }
            form_state.dispatch(HdHomeRunConfigFormAction::Devices(devices));
        })
    };

    let handle_add_device = {
        let form_state = form_state.clone();
        Callback::from(move |_| {
            let mut devices = form_state.form.devices.clone();
            let mut new_device = HdHomeRunDeviceConfigDto::default();
            let next_port = devices.iter().map(|d| d.port).max().unwrap_or(8901) + 1;
            new_device.port = next_port;
            new_device.name = format!("hdhr_{next_port}");
            if let Err(err) = new_device.prepare(devices.len() as u8, false) {
                error!("Failed to prepare hdhr device: {err}");
            }
            devices.push(new_device);
            form_state.dispatch(HdHomeRunConfigFormAction::Devices(devices));
        })
    };

    let handle_remove_device = {
        let form_state = form_state.clone();
        Callback::from(move |device_idx| {
            let mut devices = form_state.form.devices.clone();
            devices.remove(device_idx);
            form_state.dispatch(HdHomeRunConfigFormAction::Devices(devices));
        })
    };

    let render_empty = || {
        html! {
            <div class="tp__hdhomerun-config-view__body tp__config-view-page__body">
                <Card class="tp__config-view__card">
                  <h1>{translate.t(LABEL_DEVICES)}</h1>
                  <NoContent />
                </Card>
            </div>
        }
    };

    let edit_mode = *config_view_ctx.edit_mode.clone();

    let render_devices = |devices: &Vec<HdHomeRunDeviceConfigDto>| -> Html {
        if devices.is_empty() {
            render_empty()
        } else {
            html! { for devices.iter().enumerate().map(|(idx, entry)| html! {
                <HdHomerunDeviceView key={entry.port.to_string()} device_id={idx} device={entry.clone()}
                edit_mode={edit_mode} on_form_change={handle_device_change.clone()} on_remove={handle_remove_device.clone()}/>
            })}
        }
    };

    html! {
        <div class="tp__hdhomerun-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_HDHOMERUN_CONFIG)}</div>
            <div class="hdhomerun-config-view__body tp__config-view-page__header">
              {if  edit_mode {
                 html! {
                 <>
                     { edit_field_bool!(form_state, translate.t(LABEL_ENABLED), enabled, HdHomeRunConfigFormAction::Enabled) }
                     { edit_field_bool!(form_state, translate.t(LABEL_DEVICE_AUTH), auth, HdHomeRunConfigFormAction::Auth) }
                     { edit_field_bool!(form_state, translate.t(LABEL_SSDP_DISCOVERY), ssdp_discovery, HdHomeRunConfigFormAction::SsdpDiscovery) }
                     { edit_field_bool!(form_state, translate.t(LABEL_PROPRIETARY_DISCOVERY), proprietary_discovery, HdHomeRunConfigFormAction::ProprietaryDiscovery) }
                 </>
                }
              } else {
                html! {
                <>
                    { config_field_bool!(&form_state.form, translate.t(LABEL_ENABLED), enabled) }
                    { config_field_bool!(&form_state.form, translate.t(LABEL_DEVICE_AUTH), auth) }
                    { config_field_bool!(&form_state.form, translate.t(LABEL_SSDP_DISCOVERY), ssdp_discovery) }
                    { config_field_bool!(&form_state.form, translate.t(LABEL_PROPRIETARY_DISCOVERY), proprietary_discovery) }
                </>
                }
              }
              }
            </div>
            {html_if!(edit_mode, {
                <div class="tp__hdhomerun-config-view__form-action">
                    <TextButton class="primary" name="add_hdhomerun_device" title={ translate.t("LABEL.ADD_DEVICE")} onclick={handle_add_device}></TextButton>
                </div>
            })}
            <div class="hdhomerun-config-view__body tp__config-view-page__body">
              {render_devices(&form_state.form.devices)}
            </div>
        </div>
    }
}
