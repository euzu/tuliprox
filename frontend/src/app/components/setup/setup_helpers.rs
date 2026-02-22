use crate::app::{
    components::{
        config::{update_config, ConfigForm},
        InputRow, SetupConfigFormState, SetupContext, SetupStep,
    },
    ConfigContext,
};
use shared::model::{
    ApiProxyConfigDto, AppConfigDto, ConfigInputDto, ConfigTargetDto, SourcesConfigDto, TargetOutputDto, TargetUserDto,
};
use std::{collections::HashMap, rc::Rc, sync::Arc};

pub fn validate_credentials(username: &str, password: &str, password_repeat: Option<&str>) -> Result<(), &'static str> {
    if username.trim().is_empty() {
        return Err("WebUI username is required");
    }
    if password.trim().is_empty() {
        return Err("WebUI password is required");
    }
    if let Some(password_repeat) = password_repeat {
        if password != password_repeat {
            return Err("WebUI passwords do not match");
        }
    }
    Ok(())
}

pub fn format_setup_error_message(err: impl ToString) -> String {
    let mut message = err.to_string();
    if let Some(stripped) = message.strip_prefix("Tuliprox error: ") {
        message = stripped.trim().to_string();
    }

    if message.contains("HdHomeRun output is only permitted when used in combination with xtream or m3u output") {
        return "HDHomeRun output requires an additional M3U or Xtream output in the same target.".to_string();
    }
    if message.contains("HdHomeRun output has `use_output=m3u` but no `m3u` output defined") {
        return "HDHomeRun output is configured with use_output=m3u, but this target has no M3U output.".to_string();
    }
    if message.contains("HdHomeRun output has `use_output=xtream` but no `xtream` output defined") {
        return "HDHomeRun output is configured with use_output=xtream, but this target has no Xtream output."
            .to_string();
    }
    if message.contains("HdHomeRun output device is not defined") {
        return "The selected HDHomeRun device is not defined in configuration.".to_string();
    }
    if message.contains("expected expr") || message.contains("--> 1:1") {
        return "A target filter is empty or invalid. Open the target settings and set a valid filter (example: Type = live).".to_string();
    }

    message
}

pub fn collect_setup_warnings(app_config: &AppConfigDto) -> Vec<String> {
    let has_hdhr_output = app_config
        .sources
        .sources
        .iter()
        .flat_map(|source| source.targets.iter())
        .any(|target| target.output.iter().any(|output| matches!(output, TargetOutputDto::HdHomeRun(_))));
    let hdhr_disabled = app_config.config.hdhomerun.as_ref().is_some_and(|hdhr| !hdhr.enabled);

    let mut warnings = Vec::new();
    if has_hdhr_output && hdhr_disabled {
        warnings.push("You have defined an HDHomeRun output, but HDHomeRun devices are disabled.".to_string());
    }
    warnings
}

pub fn build_setup_app_config(
    config_ctx: &ConfigContext,
    form_state: &SetupConfigFormState,
    sources: shared::model::SourcesConfigDto,
) -> AppConfigDto {
    let mut app_config = config_ctx.config.as_ref().map_or_else(AppConfigDto::default, |cfg| cfg.as_ref().clone());
    if app_config.api_proxy.is_none() {
        app_config.api_proxy =
            Some(config_ctx.api_proxy.as_ref().map_or_else(ApiProxyConfigDto::default, |api| api.as_ref().clone()));
    }

    let mut modified_main_forms = Vec::new();
    for form in form_state.collect_modified_forms() {
        match form {
            ConfigForm::ApiProxy(_, api_proxy) => app_config.api_proxy = Some(api_proxy),
            ConfigForm::Panel(_, _) => {}
            other => modified_main_forms.push(other),
        }
    }

    if !modified_main_forms.is_empty() {
        update_config(&mut app_config.config, modified_main_forms);
    }

    app_config.sources = sources;
    app_config
}

pub fn prepare_config_and_api_proxy(app_config: &mut AppConfigDto) -> Result<(), String> {
    if let Err(err) = app_config.config.prepare(false) {
        return Err(err.to_string());
    }

    if let Some(api_proxy) = app_config.api_proxy.as_mut() {
        if let Err(err) = api_proxy.prepare() {
            return Err(err.to_string());
        }
    }

    Ok(())
}

pub fn prepare_sources(app_config: &mut AppConfigDto) -> Result<(), String> {
    app_config
        .sources
        .prepare(false, app_config.config.get_hdhr_device_overview().as_ref())
        .map_err(|err| err.to_string())
}

pub fn apply_setup_api_users(app_config: &mut AppConfigDto, users: &[TargetUserDto]) {
    if app_config.api_proxy.is_none() {
        app_config.api_proxy = Some(ApiProxyConfigDto::default());
    }

    if let Some(api_proxy) = app_config.api_proxy.as_mut() {
        api_proxy.user = users.to_vec();
    }
}

pub type SetupPlaylistRows = Vec<(Vec<Rc<InputRow>>, Vec<Rc<ConfigTargetDto>>)>;

pub fn map_sources_to_playlist_rows(sources: &SourcesConfigDto) -> Rc<SetupPlaylistRows> {
    let mut mapped_sources = vec![];
    let inputs_map: HashMap<Arc<str>, &ConfigInputDto> = sources.inputs.iter().map(|i| (i.name.clone(), i)).collect();

    for source in &sources.sources {
        let mut inputs = vec![];
        for input_name in &source.inputs {
            if let Some(input_cfg) = inputs_map.get(input_name) {
                let input = Rc::new((*input_cfg).clone());
                inputs.push(Rc::new(InputRow::Input(Rc::clone(&input))));
                if let Some(aliases) = input_cfg.aliases.as_ref() {
                    for alias in aliases {
                        inputs.push(Rc::new(InputRow::Alias(Rc::new(alias.clone()), Rc::clone(&input))));
                    }
                }
            }
        }

        let targets = source.targets.iter().map(|target| Rc::new(target.clone())).collect::<Vec<_>>();
        mapped_sources.push((inputs, targets));
    }

    Rc::new(mapped_sources)
}

pub fn move_to_previous_step(setup_ctx: &SetupContext, current_step: SetupStep) {
    if let Some(previous_step) = current_step.prev() {
        setup_ctx.active_step.set(previous_step);
    }
}

pub fn move_to_next_step(setup_ctx: &SetupContext, current_step: SetupStep) {
    if let Some(next_step) = current_step.next() {
        if next_step.index() > setup_ctx.max_unlocked_step.index() {
            setup_ctx.max_unlocked_step.set(next_step);
        }
        setup_ctx.active_step.set(next_step);
    }
}
