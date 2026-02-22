use crate::app::{
    components::{
        config::{update_config, ConfigForm},
        InputRow, SetupConfigFormState, SetupContext, SetupStep,
    },
    ConfigContext,
};
use log::error;
use shared::model::{
    ApiProxyConfigDto, AppConfigDto, ConfigInputDto, ConfigTargetDto, SourcesConfigDto, TargetOutputDto, TargetUserDto,
};
use std::{
    collections::{hash_map::Entry, HashMap},
    rc::Rc,
    sync::Arc,
};

pub fn validate_credentials(username: &str, password: &str, password_repeat: Option<&str>) -> Result<(), &'static str> {
    if username.trim().is_empty() {
        return Err("WebUI username is required");
    }
    if password.is_empty() {
        return Err("WebUI password is required");
    }
    if password.len() < 8 {
        return Err("WebUI password must be at least 8 characters");
    }
    if let Some(password_repeat) = password_repeat {
        if password != password_repeat {
            return Err("WebUI passwords do not match");
        }
    }
    Ok(())
}

/// Pattern map for turning backend setup validation errors into user-friendly copy.
///
/// Important: these keyword groups are matched against normalized backend error text
/// (see `normalize_setup_error_message`), so they depend on exact backend wording.
/// If backend error messages change, update the `&[&str]` entries here to match the
/// new wording, then run/extend unit tests to verify normalization + pattern matching.
const SETUP_ERROR_PATTERNS: &[(&[&str], &str)] = &[
    (
        &["hdhomerun output is only permitted when used in combination with xtream or m3u output"],
        "HDHomeRun output requires an additional M3U or Xtream output in the same target.",
    ),
    (
        &["hdhomerun output has `use_output=m3u`", "no `m3u` output defined"],
        "HDHomeRun output is configured with use_output=m3u, but this target has no M3U output.",
    ),
    (
        &["hdhomerun output has `use_output=xtream`", "no `xtream` output defined"],
        "HDHomeRun output is configured with use_output=xtream, but this target has no Xtream output.",
    ),
    (&["hdhomerun output device is not defined"], "The selected HDHomeRun device is not defined in configuration."),
    (
        &["expected expr"],
        "A target filter is empty or invalid. Open the target settings and set a valid filter (example: Type = live).",
    ),
    (
        &["--> 1:1"],
        "A target filter is empty or invalid. Open the target settings and set a valid filter (example: Type = live).",
    ),
];

fn normalize_setup_error_message(message: &str) -> String {
    message.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn matches_setup_error_pattern(message: &str, pattern_keywords: &[&str]) -> bool {
    pattern_keywords.iter().all(|keyword| message.contains(keyword))
}

pub fn format_setup_error_message(err: impl ToString) -> String {
    // Keep this mapping in sync with backend setup error text. If backend wording
    // changes, update the matching keyword slices in `SETUP_ERROR_PATTERNS` and
    // extend tests so regressions are caught early in CI.
    let mut message = err.to_string();
    if let Some(stripped) = message.strip_prefix("Tuliprox error: ") {
        message = stripped.trim().to_string();
    }

    let normalized_message = normalize_setup_error_message(&message);
    for (pattern_keywords, user_message) in SETUP_ERROR_PATTERNS {
        if matches_setup_error_pattern(&normalized_message, pattern_keywords) {
            return (*user_message).to_string();
        }
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
    let api_proxy = app_config.api_proxy.get_or_insert_with(ApiProxyConfigDto::default);
    api_proxy.user = users.to_vec();
}

pub type SetupPlaylistRows = Vec<(Vec<Rc<InputRow>>, Vec<Rc<ConfigTargetDto>>)>;

pub fn map_sources_to_playlist_rows(sources: &SourcesConfigDto) -> Rc<SetupPlaylistRows> {
    let mut mapped_sources = vec![];
    let mut inputs_map: HashMap<Arc<str>, &ConfigInputDto> = HashMap::with_capacity(sources.inputs.len());
    for input in &sources.inputs {
        match inputs_map.entry(input.name.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(input);
            }
            Entry::Occupied(existing) => {
                debug_assert!(false, "Duplicate input name found in setup sources: {}", input.name);
                error!(
                    "Duplicate input name in setup sources: '{}' (existing id: {}, duplicate id: {})",
                    input.name,
                    existing.get().id,
                    input.id
                );
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::{
        format_setup_error_message, matches_setup_error_pattern, normalize_setup_error_message, SETUP_ERROR_PATTERNS,
    };

    #[test]
    fn normalizes_and_matches_known_setup_error_pattern() {
        let raw = "  HDHomerun   output has `use_output=xtream` and no `xtream` output defined  ";
        let normalized = normalize_setup_error_message(raw);
        let (keywords, _) = SETUP_ERROR_PATTERNS
            .iter()
            .find(|(keywords, _)| keywords.contains(&"hdhomerun output has `use_output=xtream`"))
            .expect("Xtream setup error pattern must exist");

        assert!(matches_setup_error_pattern(&normalized, keywords));
    }

    #[test]
    fn formats_known_backend_error_to_user_friendly_message() {
        let mapped = format_setup_error_message(
            "Tuliprox error: HDHomerun output has `use_output=xtream` and no `xtream` output defined",
        );

        assert_eq!(
            mapped,
            "HDHomeRun output is configured with use_output=xtream, but this target has no Xtream output."
        );
    }
}
