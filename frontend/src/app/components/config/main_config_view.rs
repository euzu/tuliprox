use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::MainConfigDto;
use crate::app::context::ConfigContext;
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::config::config_page::{ConfigForm, LABEL_MAIN_CONFIG};
use crate::{config_field_optional, config_field_bool, config_field, edit_field_text_option, edit_field_bool,
            generate_form_reducer, edit_field_number, edit_field_number_option, edit_field_text};

const LABEL_UPDATE_ON_BOOT: &str = "LABEL.UPDATE_ON_BOOT";
const LABEL_CONFIG_HOT_RELOAD: &str = "LABEL.CONFIG_HOT_RELOAD";
const LABEL_USER_ACCESS_CONTROL: &str = "LABEL.USER_ACCESS_CONTROL";
const LABEL_PROCESS_PARALLEL: &str = "LABEL.PROCESS_PARALLEL";
const LABEL_DISK_BASED_PROCESSING: &str = "LABEL.DISK_BASED_PROCESSING";
const LABEL_WORKING_DIR: &str = "LABEL.WORKING_DIR";
const LABEL_MAPPING_PATH: &str = "LABEL.MAPPING_PATH";
const LABEL_BACKUP_DIR: &str = "LABEL.BACKUP_DIR";
const LABEL_USER_CONFIG_DIR: &str = "LABEL.USER_CONFIG_DIR";
const LABEL_SLEEP_TIMER_MINS: &str = "LABEL.SLEEP_TIMER_MINS";
const LABEL_CONNECT_TIMEOUT_SECS: &str = "LABEL.CONNECT_TIMEOUT_SECS";
const LABEL_CUSTOM_STREAM_RESPONSE_PATH: &str = "LABEL.CUSTOM_STREAM_RESPONSE_PATH";
const LABEL_ACCEPT_INSECURE_SSL_CERTIFICATES: &str = "LABEL.ACCEPT_INSECURE_SSL_CERTIFICATES";

generate_form_reducer!(
    state: MainConfigFormState { form: MainConfigDto },
    action_name: MainConfigFormAction,
    fields {
        UpdateOnBoot => update_on_boot: bool,
        ConfigHotReload => config_hot_reload: bool,
        UserAccessControl => user_access_control: bool,
        AcceptInsecureSslCertificates => accept_insecure_ssl_certificates: bool,
        ProcessParallel => process_parallel: bool,
        DiskBasedProcessing => disk_based_processing: bool,
        WorkingDir => working_dir: String,
        MappingPath => mapping_path: Option<String>,
        BackupDir => backup_dir: Option<String>,
        UserConfigDir => user_config_dir: Option<String>,
        SleepTimerMins => sleep_timer_mins: Option<u32>,
        ConnectTimeoutSecs => connect_timeout_secs: u32,
        CustomStreamResponsePath => custom_stream_response_path: Option<String>,
    }
);

#[function_component]
pub fn MainConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<MainConfigFormState> = use_reducer(|| {
        MainConfigFormState { form: MainConfigDto::default(), modified: false }
    });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::Main(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let config = config_ctx.config.as_ref().map(|c| c.config.clone());
        use_effect_with((config, config_view_ctx.edit_mode.clone()), move |(cfg, _mode)| {
            if let Some(main) = cfg {
                let main_config =  MainConfigDto::from(main);
                form_state.dispatch(MainConfigFormAction::SetAll(main_config.clone()));
            } else {
                form_state.dispatch(MainConfigFormAction::SetAll(MainConfigDto::default()));
            }
            || ()
        });
    }

    let render_view_mode = || {
        html! {
            <>
                { config_field_bool!(form_state.form, translate.t(LABEL_UPDATE_ON_BOOT), update_on_boot) }
                { config_field_bool!(form_state.form, translate.t(LABEL_CONFIG_HOT_RELOAD), config_hot_reload) }
                { config_field_bool!(form_state.form, translate.t(LABEL_USER_ACCESS_CONTROL), user_access_control) }
                { config_field_bool!(form_state.form, translate.t(LABEL_ACCEPT_INSECURE_SSL_CERTIFICATES), accept_insecure_ssl_certificates) }
                { config_field_bool!(form_state.form, translate.t(LABEL_PROCESS_PARALLEL), process_parallel) }
                { config_field_bool!(form_state.form, translate.t(LABEL_DISK_BASED_PROCESSING), disk_based_processing) }
                { config_field!(form_state.form, translate.t(LABEL_WORKING_DIR), working_dir) }
                { config_field_optional!(form_state.form, translate.t(LABEL_MAPPING_PATH), mapping_path) }
                { config_field_optional!(form_state.form, translate.t(LABEL_BACKUP_DIR), backup_dir) }
                { config_field_optional!(form_state.form, translate.t(LABEL_USER_CONFIG_DIR), user_config_dir) }
                { config_field_optional!(form_state.form, translate.t(LABEL_SLEEP_TIMER_MINS), sleep_timer_mins) }
                { config_field!(form_state.form, translate.t(LABEL_CONNECT_TIMEOUT_SECS), connect_timeout_secs) }
                { config_field_optional!(form_state.form, translate.t(LABEL_CUSTOM_STREAM_RESPONSE_PATH), custom_stream_response_path) }
            </>
        }
    };

    let render_edit_mode = || html! {
        <>
            { edit_field_bool!(form_state, translate.t(LABEL_UPDATE_ON_BOOT), update_on_boot, MainConfigFormAction::UpdateOnBoot) }
            { edit_field_bool!(form_state, translate.t(LABEL_CONFIG_HOT_RELOAD), config_hot_reload, MainConfigFormAction::ConfigHotReload) }
            { edit_field_bool!(form_state, translate.t(LABEL_USER_ACCESS_CONTROL), user_access_control, MainConfigFormAction::UserAccessControl) }
            { edit_field_bool!(form_state, translate.t(LABEL_ACCEPT_INSECURE_SSL_CERTIFICATES), accept_insecure_ssl_certificates, MainConfigFormAction::AcceptInsecureSslCertificates) }
            { edit_field_bool!(form_state, translate.t(LABEL_PROCESS_PARALLEL), process_parallel, MainConfigFormAction::ProcessParallel) }
            { edit_field_bool!(form_state, translate.t(LABEL_DISK_BASED_PROCESSING), disk_based_processing, MainConfigFormAction::DiskBasedProcessing) }
            { edit_field_text!(form_state, translate.t(LABEL_WORKING_DIR), working_dir, MainConfigFormAction::WorkingDir) }
            { edit_field_text_option!(form_state, translate.t(LABEL_MAPPING_PATH), mapping_path, MainConfigFormAction::MappingPath) }
            { edit_field_text_option!(form_state, translate.t(LABEL_BACKUP_DIR), backup_dir, MainConfigFormAction::BackupDir) }
            { edit_field_text_option!(form_state, translate.t(LABEL_USER_CONFIG_DIR), user_config_dir, MainConfigFormAction::UserConfigDir) }
            { edit_field_number_option!(form_state, translate.t(LABEL_SLEEP_TIMER_MINS), sleep_timer_mins, MainConfigFormAction::SleepTimerMins) }
            { edit_field_number!(form_state, translate.t(LABEL_CONNECT_TIMEOUT_SECS), connect_timeout_secs, MainConfigFormAction::ConnectTimeoutSecs) }
            { edit_field_text_option!(form_state, translate.t(LABEL_CUSTOM_STREAM_RESPONSE_PATH), custom_stream_response_path, MainConfigFormAction::CustomStreamResponsePath) }
        </>
    };

    html! {
        <div class="tp__main-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_MAIN_CONFIG)}</div>
            <div class="tp__main-config-view__body tp__config-view-page__body">
                {
                    if *config_view_ctx.edit_mode {
                        render_edit_mode()
                    } else {
                        render_view_mode()
                    }
                }
            </div>
        </div>
    }
}
