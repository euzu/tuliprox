use crate::app::{
    components::{config::ConfigForm, SetupConfigFormState, SetupContext, SetupStep},
    ConfigContext,
};
use shared::{
    model::{
        ApiProxyConfigDto, AppConfigDto, HdHomeRunConfigDto, LibraryConfigDto, LibraryMetadataConfigDto,
        LibraryPlaylistConfigDto, LogConfigDto, ReverseProxyConfigDto, ReverseProxyDisabledHeaderConfigDto,
        StreamConfigDto, TargetOutputDto, TargetUserDto, ThumbnailConfigDto, WebUiConfigDto,
    },
    utils::{default_secret, is_default_supported_library_extensions},
};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    MissingUsername,
    MissingPassword,
    PasswordTooShort,
    PasswordMismatch,
}

impl ValidationError {
    pub const fn i18n_key(self) -> &'static str {
        match self {
            Self::MissingUsername => "SETUP.MSG.WEBUI_USERNAME_REQUIRED",
            Self::MissingPassword => "SETUP.MSG.WEBUI_PASSWORD_REQUIRED",
            Self::PasswordTooShort => "SETUP.MSG.WEBUI_PASSWORD_MIN_LENGTH",
            Self::PasswordMismatch => "SETUP.MSG.WEBUI_PASSWORDS_DO_NOT_MATCH",
        }
    }
}

pub fn validate_credentials(
    username: &str,
    password: &str,
    password_repeat: Option<&str>,
) -> Result<(), ValidationError> {
    if username.trim().is_empty() {
        return Err(ValidationError::MissingUsername);
    }
    if password.is_empty() {
        return Err(ValidationError::MissingPassword);
    }
    if password.len() < 8 {
        return Err(ValidationError::PasswordTooShort);
    }
    if let Some(password_repeat) = password_repeat {
        if password != password_repeat {
            return Err(ValidationError::PasswordMismatch);
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
    let mut result = String::new();
    for part in message.trim().to_lowercase().split_whitespace() {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(part);
    }
    result
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

    let modified_forms = form_state.collect_modified_forms();
    let reverse_proxy_modified = modified_forms.iter().any(|form| matches!(form, ConfigForm::ReverseProxy(_, _)));
    let log_modified = modified_forms.iter().any(|form| matches!(form, ConfigForm::Log(_, _)));
    let setup_rewrite_secret = form_state.slots.reverse_proxy.as_ref().and_then(|form| match form {
        ConfigForm::ReverseProxy(_, cfg) if !cfg.rewrite_secret.trim().is_empty() => Some(cfg.rewrite_secret.clone()),
        _ => None,
    });

    let mut modified_main_forms = Vec::new();
    for form in modified_forms {
        match form {
            ConfigForm::ApiProxy(_, api_proxy) => app_config.api_proxy = Some(api_proxy),
            ConfigForm::Panel(_, _) => {}
            other => modified_main_forms.push(other),
        }
    }

    if !modified_main_forms.is_empty() {
        apply_setup_config_forms(&mut app_config.config, modified_main_forms);
    }

    apply_setup_main_defaults(&mut app_config);
    apply_setup_log_defaults(&mut app_config, !log_modified);
    apply_setup_reverse_proxy_defaults(&mut app_config, !reverse_proxy_modified, setup_rewrite_secret.as_deref());
    app_config.sources = sources;
    app_config
}

fn apply_setup_config_forms(config: &mut shared::model::ConfigDto, forms: Vec<ConfigForm>) {
    for form in forms {
        match form {
            ConfigForm::Main(_, main_cfg) => config.update_from_main_config(&main_cfg),
            ConfigForm::Api(_, api_cfg) => config.api = api_cfg,
            ConfigForm::Log(_, log_cfg) => config.log = Some(log_cfg),
            ConfigForm::Schedules(_, schedules_cfg) => config.schedules = schedules_cfg.schedules,
            ConfigForm::Video(_, video_cfg) => config.video = Some(video_cfg),
            ConfigForm::MetadataUpdate(_, mut metadata_update_cfg) => {
                if metadata_update_cfg.is_empty() {
                    config.metadata_update = None;
                } else {
                    metadata_update_cfg.clean();
                    config.metadata_update = Some(metadata_update_cfg);
                }
            }
            ConfigForm::Messaging(_, messaging_cfg) => config.messaging = Some(messaging_cfg),
            ConfigForm::WebUi(modified, web_ui_cfg) => apply_setup_webui_form(config, web_ui_cfg, modified),
            ConfigForm::ReverseProxy(_, reverse_proxy_cfg) => config.reverse_proxy = Some(reverse_proxy_cfg),
            ConfigForm::HdHomerun(_, hdhr_cfg) => apply_setup_hdhomerun_form(config, hdhr_cfg),
            ConfigForm::Proxy(_, proxy_cfg) => config.proxy = Some(proxy_cfg),
            ConfigForm::IpCheck(_, ipcheck_cfg) => config.ipcheck = Some(ipcheck_cfg),
            ConfigForm::Library(_, library_cfg) => apply_setup_library_form(config, library_cfg),
            ConfigForm::Panel(_, _) => {}
            ConfigForm::ApiProxy(_, _) => {}
        }
    }
}

fn is_setup_library_toggle_only_update(cfg: &LibraryConfigDto) -> bool {
    cfg.scan_directories.is_empty()
        && (cfg.supported_extensions.is_empty() || is_default_supported_library_extensions(&cfg.supported_extensions))
        && cfg.metadata == LibraryMetadataConfigDto::default()
        && cfg.playlist == LibraryPlaylistConfigDto::default()
        && cfg.thumbnails == ThumbnailConfigDto::default()
}

fn apply_setup_hdhomerun_form(config: &mut shared::model::ConfigDto, hdhr_cfg: HdHomeRunConfigDto) {
    if hdhr_cfg.is_empty() {
        config.hdhomerun = None;
    } else {
        config.hdhomerun = Some(hdhr_cfg);
    }
}

fn apply_setup_library_form(config: &mut shared::model::ConfigDto, library_cfg: LibraryConfigDto) {
    if let Some(existing) = config.library.as_mut() {
        if is_setup_library_toggle_only_update(&library_cfg) {
            existing.enabled = library_cfg.enabled;
            return;
        }
    }
    config.library = Some(library_cfg);
}

fn apply_setup_webui_form(config: &mut shared::model::ConfigDto, web_ui_cfg: WebUiConfigDto, modified: bool) {
    // If there's no existing web_ui config: clean and store the form (empty form -> None).
    if config.web_ui.is_none() {
        if web_ui_cfg.is_empty() {
            return;
        }
        let mut cfg = web_ui_cfg;
        cfg.clean();
        config.web_ui = Some(cfg);
        return;
    }

    if !modified && web_ui_cfg.is_empty() {
        return;
    }

    if modified && web_ui_cfg.is_empty() {
        config.web_ui = None;
    } else {
        let mut cfg = web_ui_cfg;
        cfg.clean();
        config.web_ui = Some(cfg);
    }
}

fn apply_setup_main_defaults(app_config: &mut AppConfigDto) {
    app_config.config.user_access_control = true;
    app_config.config.config_hot_reload = true;
}

fn apply_setup_log_defaults(app_config: &mut AppConfigDto, initialize_defaults: bool) {
    if !initialize_defaults {
        return;
    }

    let log_config = app_config
        .config
        .log
        .get_or_insert_with(|| LogConfigDto { log_level: Some("INFO".to_string()), ..Default::default() });
    if log_config.log_level.as_ref().is_none_or(|level| level.trim().is_empty()) {
        log_config.log_level = Some("INFO".to_string());
    }
}

fn setup_default_rewrite_secret() -> String {
    static SETUP_REWRITE_SECRET: OnceLock<String> = OnceLock::new();
    SETUP_REWRITE_SECRET.get_or_init(default_secret).clone()
}

fn setup_default_disabled_header_config() -> ReverseProxyDisabledHeaderConfigDto {
    ReverseProxyDisabledHeaderConfigDto {
        referer_header: true,
        x_header: true,
        cloudflare_header: true,
        custom_header: Vec::new(),
    }
}

fn apply_setup_reverse_proxy_defaults(
    app_config: &mut AppConfigDto,
    initialize_optional_defaults: bool,
    setup_rewrite_secret: Option<&str>,
) {
    let setup_rewrite_secret =
        setup_rewrite_secret.map(str::trim).filter(|secret| !secret.is_empty()).map(ToString::to_string);

    if !initialize_optional_defaults {
        if let Some(reverse_proxy) = app_config.config.reverse_proxy.as_mut() {
            if reverse_proxy.rewrite_secret.trim().is_empty() {
                reverse_proxy.rewrite_secret = setup_rewrite_secret.unwrap_or_else(setup_default_rewrite_secret);
            }
        }
        return;
    }

    let reverse_proxy = app_config.config.reverse_proxy.get_or_insert_with(|| ReverseProxyConfigDto {
        rewrite_secret: setup_rewrite_secret.clone().unwrap_or_else(setup_default_rewrite_secret),
        stream: Some(StreamConfigDto { retry: true, ..Default::default() }),
        disabled_header: Some(setup_default_disabled_header_config()),
        ..Default::default()
    });

    if reverse_proxy.stream.is_none() || reverse_proxy.stream.as_ref().is_some_and(StreamConfigDto::is_empty) {
        reverse_proxy.stream = Some(StreamConfigDto { retry: true, ..Default::default() });
    }

    if reverse_proxy.disabled_header.as_ref().is_none_or(ReverseProxyDisabledHeaderConfigDto::is_empty) {
        reverse_proxy.disabled_header = Some(setup_default_disabled_header_config());
    }

    if reverse_proxy.rewrite_secret.trim().is_empty() {
        reverse_proxy.rewrite_secret = setup_rewrite_secret.unwrap_or_else(setup_default_rewrite_secret);
    }
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
        .prepare(
            false,
            app_config.config.get_hdhr_device_overview().as_ref(),
            app_config.templates.as_ref().map(|defs| defs.templates.as_slice()),
        )
        .map_err(|err| err.to_string())
}

pub fn apply_setup_api_users(app_config: &mut AppConfigDto, users: &[TargetUserDto]) {
    let api_proxy = app_config.api_proxy.get_or_insert_with(ApiProxyConfigDto::default);
    api_proxy.user = users.to_vec();
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
        apply_setup_log_defaults, apply_setup_main_defaults, apply_setup_reverse_proxy_defaults,
        build_setup_app_config, format_setup_error_message, matches_setup_error_pattern, normalize_setup_error_message,
        ConfigForm, SETUP_ERROR_PATTERNS,
    };
    use crate::app::{components::map_sources_to_playlist_rows, ConfigContext};
    use shared::model::{
        view_type::ViewType, AppConfigDto, ConfigInputDto, ContentSecurityPolicyConfigDto, HdHomeRunConfigDto,
        HdHomeRunDeviceConfigDto, LibraryConfigDto, LibraryScanDirectoryDto, MetadataUpdateConfigDto,
        ReverseProxyConfigDto, SourcesConfigDto, StreamInfoConfigDto, WebAuthConfigDto, WebUiConfigDto,
    };
    use std::rc::Rc;

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

    #[test]
    fn setup_defaults_initialize_reverse_proxy_defaults_when_unset() {
        let mut app_config = AppConfigDto::default();

        apply_setup_reverse_proxy_defaults(&mut app_config, true, None);

        let reverse_proxy = app_config.config.reverse_proxy.expect("reverse_proxy must be initialized");
        assert!(!reverse_proxy.rewrite_secret.is_empty());
        assert_eq!(reverse_proxy.stream.as_ref().map(|stream| stream.retry), Some(true));
        assert_eq!(
            reverse_proxy.disabled_header.as_ref().map(|header| (
                header.referer_header,
                header.x_header,
                header.cloudflare_header
            )),
            Some((true, true, true))
        );
    }

    #[test]
    fn setup_defaults_do_not_recreate_reverse_proxy_when_user_modified_it_to_empty() {
        let mut app_config = AppConfigDto::default();
        app_config.config.reverse_proxy = Some(ReverseProxyConfigDto::default());

        apply_setup_reverse_proxy_defaults(&mut app_config, false, None);

        let reverse_proxy = app_config.config.reverse_proxy.expect("reverse_proxy remains present");
        assert!(!reverse_proxy.rewrite_secret.is_empty());
        assert_eq!(reverse_proxy.stream, None);
        assert_eq!(reverse_proxy.disabled_header, None);
    }

    #[test]
    fn setup_defaults_reuse_existing_setup_secret_to_avoid_regeneration_loops() {
        let mut app_config = AppConfigDto::default();
        let secret = "AABBCCDDEEFF00112233445566778899";

        apply_setup_reverse_proxy_defaults(&mut app_config, true, Some(secret));

        let reverse_proxy = app_config.config.reverse_proxy.expect("reverse_proxy must be initialized");
        assert_eq!(reverse_proxy.rewrite_secret, secret);
    }

    #[test]
    fn setup_defaults_enable_user_access_control() {
        let mut app_config = AppConfigDto::default();
        assert!(!app_config.config.user_access_control);
        assert!(!app_config.config.config_hot_reload);

        apply_setup_main_defaults(&mut app_config);

        assert!(app_config.config.user_access_control);
        assert!(app_config.config.config_hot_reload);
    }

    #[test]
    fn setup_defaults_set_log_level_to_info_when_empty() {
        let mut app_config = AppConfigDto::default();
        assert!(app_config.config.log.is_none());

        apply_setup_log_defaults(&mut app_config, true);

        let log = app_config.config.log.expect("log config should be initialized");
        assert_eq!(log.log_level.as_deref(), Some("INFO"));
    }

    #[test]
    fn setup_defaults_keep_rewrite_secret_stable_across_rebuilds_without_reverse_proxy_form() {
        let config_ctx = ConfigContext { config: Some(Rc::new(AppConfigDto::default())), api_proxy: None };
        let form_state = crate::app::components::setup::SetupConfigFormState::default();
        let sources = shared::model::SourcesConfigDto::default();

        let app_cfg_first = build_setup_app_config(&config_ctx, &form_state, sources.clone());
        let app_cfg_second = build_setup_app_config(&config_ctx, &form_state, sources);

        let first_secret =
            app_cfg_first.config.reverse_proxy.as_ref().expect("reverse_proxy must be present").rewrite_secret.clone();
        let second_secret =
            app_cfg_second.config.reverse_proxy.as_ref().expect("reverse_proxy must be present").rewrite_secret.clone();

        assert_eq!(first_secret, second_secret);
        assert!(!first_secret.is_empty());
    }

    #[test]
    fn map_sources_to_playlist_rows_handles_duplicate_empty_input_names_without_panicking() {
        use shared::{
            model::{ConfigSourceDto, InputType},
            utils::Internable,
        };

        let sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto { name: "".intern(), input_type: InputType::Xtream, ..Default::default() },
                ConfigInputDto { name: "".intern(), input_type: InputType::M3u, ..Default::default() },
            ],
            sources: vec![ConfigSourceDto { inputs: vec!["".intern(), "".intern()], targets: vec![] }],
            ..Default::default()
        };

        let rows = map_sources_to_playlist_rows(&sources);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.len(), 2);
    }

    #[test]
    fn setup_library_form_keeps_payload_when_disabled() {
        let config_ctx = ConfigContext { config: Some(Rc::new(AppConfigDto::default())), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        let mut library = LibraryConfigDto { enabled: false, ..Default::default() };
        library
            .scan_directories
            .push(shared::model::LibraryScanDirectoryDto { path: "/media".to_string(), ..Default::default() });

        form_state.update_form(ConfigForm::Library(true, library.clone()));
        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());

        assert_eq!(app_cfg.config.library, Some(library));
    }

    #[test]
    fn setup_library_toggle_only_form_preserves_existing_payload() {
        let mut app_config = AppConfigDto::default();
        app_config.config.library = Some(LibraryConfigDto {
            enabled: true,
            scan_directories: vec![LibraryScanDirectoryDto {
                enabled: true,
                path: "/media".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::Library(true, LibraryConfigDto { enabled: false, ..Default::default() }));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let library = app_cfg.config.library.expect("library config should be present");
        assert!(!library.enabled);
        assert!(!library.thumbnails.enabled);
        assert_eq!(library.scan_directories.len(), 1);
        assert_eq!(library.scan_directories[0].path, "/media");
    }

    #[test]
    fn setup_hdhomerun_empty_form_clears_existing_payload() {
        let mut app_config = AppConfigDto::default();
        app_config.config.hdhomerun = Some(HdHomeRunConfigDto {
            enabled: true,
            devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::HdHomerun(true, HdHomeRunConfigDto::default()));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        assert!(app_cfg.config.hdhomerun.is_none(), "empty forms must clear existing HDHomeRun payload");
    }

    #[test]
    fn setup_hdhomerun_form_allows_removing_all_devices() {
        let mut app_config = AppConfigDto::default();
        app_config.config.hdhomerun = Some(HdHomeRunConfigDto {
            enabled: true,
            devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::HdHomerun(
            true,
            HdHomeRunConfigDto { enabled: true, devices: Vec::new(), ..Default::default() },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let hdhr = app_cfg.config.hdhomerun.expect("hdhomerun config should be present");
        assert!(hdhr.enabled);
        assert!(hdhr.devices.is_empty(), "devices should be removed instead of restored");
    }

    #[test]
    fn setup_hdhomerun_form_allows_removing_all_devices_while_disabled() {
        let mut app_config = AppConfigDto::default();
        app_config.config.hdhomerun = Some(HdHomeRunConfigDto {
            enabled: false,
            devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::HdHomerun(true, HdHomeRunConfigDto::default()));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        assert!(app_cfg.config.hdhomerun.is_none(), "disabled empty forms should clear the HDHomeRun config");
    }

    #[test]
    fn setup_webui_toggle_only_form_preserves_existing_payload() {
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            user_ui_enabled: true,
            path: Some("/dashboard".to_string()),
            player_server: Some("http://player.local".to_string()),
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "tuliprox".to_string(),
                secret: "top-secret".to_string(),
                groupfile: Some("groups.txt".to_string()),
                ..Default::default()
            }),
            content_security_policy: Some(ContentSecurityPolicyConfigDto {
                enabled: true,
                custom_attributes: Some(vec!["default-src 'self'".to_string()]),
            }),
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true,
            WebUiConfigDto {
                enabled: false,
                user_ui_enabled: false,
                path: Some("/dashboard".to_string()),
                player_server: Some("http://player.local".to_string()),
                auth: Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "tuliprox".to_string(),
                    secret: "top-secret".to_string(),
                    groupfile: Some("groups.txt".to_string()),
                    ..Default::default()
                }),
                content_security_policy: Some(ContentSecurityPolicyConfigDto {
                    enabled: true,
                    custom_attributes: Some(vec!["default-src 'self'".to_string()]),
                }),
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert!(!web_ui.user_ui_enabled);
        assert_eq!(web_ui.path.as_deref(), Some("/dashboard"));
        assert_eq!(web_ui.player_server.as_deref(), Some("http://player.local"));
        assert_eq!(web_ui.auth.as_ref().map(|auth| auth.secret.as_str()), Some("top-secret"));
        assert_eq!(web_ui.auth.as_ref().and_then(|auth| auth.groupfile.as_deref()), Some("groups.txt"));
    }

    #[test]
    fn setup_webui_explicit_stream_info_clear() {
        // User explicitly clears all hide_* flags via the form (stream_info=None).
        // modified=true signals explicit edit; existing stream_info must become None.
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            stream_info: Some(StreamInfoConfigDto { hide_ip: true, ..Default::default() }),
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true, // modified=true: explicit edit
            WebUiConfigDto {
                enabled: false,
                stream_info: None, // user cleared all flags
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert!(web_ui.stream_info.is_none(), "stream_info should be explicitly cleared to None");
    }

    #[test]
    fn setup_webui_explicit_landing_page_reset_to_default() {
        // User explicitly resets landing_page to default while also changing scalars.
        // The reset landing_page value must be applied literally.
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            landing_page: ViewType::Streams, // existing is non-default
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true, // modified=true: explicit edit
            WebUiConfigDto {
                enabled: false,
                landing_page: ViewType::Dashboard, // reset to default
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, ViewType::Dashboard, "landing_page reset to default must be applied");
    }

    #[test]
    fn setup_webui_toggle_only_preserves_stream_info() {
        // Toggle-only edit with a real synced form snapshot: user changes scalars, untouched
        // stream_info is emitted literally from the existing config and must be preserved.
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            stream_info: Some(StreamInfoConfigDto { hide_ip: true, ..Default::default() }),
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true,
            WebUiConfigDto {
                enabled: false,
                stream_info: Some(StreamInfoConfigDto { hide_ip: true, ..Default::default() }),
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        let stream_info = web_ui.stream_info.expect("stream_info should be preserved on toggle-only");
        assert!(stream_info.hide_ip);
    }

    #[test]
    fn setup_webui_toggle_only_preserves_landing_page() {
        // Toggle-only sparse update (modified=true): landing_page in form matches existing
        // (form syncs landing_page from existing on open). Landing_page should be preserved.
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            landing_page: ViewType::Streams, // existing is non-default
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true, // modified=true: form IS applied
            WebUiConfigDto {
                enabled: false,
                landing_page: ViewType::Streams, // form syncs from existing on open
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, ViewType::Streams, "landing_page should be preserved on toggle-only");
    }

    #[test]
    fn setup_webui_toggle_only_with_nested_field_change_does_selective_update() {
        // User changed auth issuer (nested field change), landing_page stays at synced value.
        // modified=true means this is a real form edit and nested values must be applied literally.
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            landing_page: ViewType::Streams,
            auth: Some(WebAuthConfigDto { issuer: "test".to_string(), ..Default::default() }),
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true, // explicit edit: nested values must be applied
            WebUiConfigDto {
                enabled: false,
                landing_page: ViewType::Streams, // form syncs from existing
                auth: Some(WebAuthConfigDto { issuer: "other".to_string(), ..Default::default() }),
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, ViewType::Streams);
        assert_eq!(web_ui.auth.as_ref().map(|a| a.issuer.as_str()), Some("other"));
    }

    #[test]
    fn setup_webui_explicit_clear_auth_path_player_server_and_reset_kick_secs() {
        let mut app_config = AppConfigDto::default();
        app_config.config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            path: Some("/dashboard".to_string()),
            player_server: Some("http://player.local".to_string()),
            kick_secs: 321,
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "issuer".to_string(),
                secret: "top-secret".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::WebUi(
            true,
            WebUiConfigDto {
                enabled: false,
                path: None,
                player_server: None,
                kick_secs: WebUiConfigDto::default().kick_secs,
                auth: Some(WebAuthConfigDto::default()),
                ..Default::default()
            },
        ));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let web_ui = app_cfg.config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert!(web_ui.path.is_none());
        assert!(web_ui.player_server.is_none());
        assert_eq!(web_ui.kick_secs, WebUiConfigDto::default().kick_secs);
        assert!(web_ui.auth.is_none());
    }

    #[test]
    fn setup_metadata_update_empty_form_clears_existing_payload() {
        let mut app_config = AppConfigDto::default();
        let mut metadata_update = MetadataUpdateConfigDto::default();
        metadata_update.ffprobe.enabled = true;
        app_config.config.metadata_update = Some(metadata_update);
        let config_ctx = ConfigContext { config: Some(Rc::new(app_config)), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        form_state.update_form(ConfigForm::MetadataUpdate(true, MetadataUpdateConfigDto::default()));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        assert!(app_cfg.config.metadata_update.is_none());
    }

    #[test]
    fn setup_metadata_update_form_applies_cleaned_payload() {
        let config_ctx = ConfigContext { config: Some(Rc::new(AppConfigDto::default())), api_proxy: None };
        let mut form_state = crate::app::components::setup::SetupConfigFormState::default();
        let mut metadata_update = MetadataUpdateConfigDto::default();
        metadata_update.ffprobe.enabled = true;
        metadata_update.ffprobe.timeout = Some(60);
        form_state.update_form(ConfigForm::MetadataUpdate(true, metadata_update));

        let app_cfg = build_setup_app_config(&config_ctx, &form_state, SourcesConfigDto::default());
        let metadata_update = app_cfg.config.metadata_update.expect("metadata_update config should be present");
        assert!(metadata_update.ffprobe.enabled);
        assert_eq!(metadata_update.ffprobe.timeout, Some(60));
    }
}
