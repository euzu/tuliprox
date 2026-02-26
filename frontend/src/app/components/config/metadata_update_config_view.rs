use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_METADATA_UPDATE_CONFIG},
                config_view_context::ConfigViewContext,
            },
            dto_field_id,
            number_input::NumberInput,
            Card,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_optional, edit_field_bool, edit_field_number,
    edit_field_number_option_u64, edit_field_number_u64, edit_field_number_u8, edit_field_number_usize,
    edit_field_text, edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{
    FfprobeConfigDto, MetadataLogConfigDto, MetadataUpdateConfigDto, ProbeConfigDto, ResolveConfigDto, TmdbConfigDto,
};
use yew::prelude::*;

const LABEL_QUEUE_LOG_INTERVAL: &str = "LABEL.METADATA_QUEUE_LOG_INTERVAL";
const LABEL_PROGRESS_LOG_INTERVAL: &str = "LABEL.METADATA_PROGRESS_LOG_INTERVAL";
const LABEL_MAX_RESOLVE_RETRY_BACKOFF: &str = "LABEL.METADATA_MAX_RESOLVE_RETRY_BACKOFF";
const LABEL_RESOLVE_MIN_RETRY_BASE: &str = "LABEL.METADATA_RESOLVE_MIN_RETRY_BASE";
const LABEL_RESOLVE_EXHAUSTION_RESET_GAP: &str = "LABEL.METADATA_RESOLVE_EXHAUSTION_RESET_GAP";
const LABEL_PROBE_COOLDOWN: &str = "LABEL.METADATA_PROBE_COOLDOWN";
const LABEL_TMDB_COOLDOWN: &str = "LABEL.METADATA_TMDB_COOLDOWN";
const LABEL_RETRY_DELAY: &str = "LABEL.METADATA_RETRY_DELAY";
const LABEL_PROBE_RETRY_LOAD_RETRY_DELAY: &str = "LABEL.METADATA_PROBE_RETRY_LOAD_RETRY_DELAY";
const LABEL_WORKER_IDLE_TIMEOUT: &str = "LABEL.METADATA_WORKER_IDLE_TIMEOUT";
const LABEL_PROBE_RETRY_BACKOFF_STEP_1: &str = "LABEL.METADATA_PROBE_RETRY_BACKOFF_STEP_1";
const LABEL_PROBE_RETRY_BACKOFF_STEP_2: &str = "LABEL.METADATA_PROBE_RETRY_BACKOFF_STEP_2";
const LABEL_PROBE_RETRY_BACKOFF_STEP_3: &str = "LABEL.METADATA_PROBE_RETRY_BACKOFF_STEP_3";
const LABEL_MAX_ATTEMPTS_RESOLVE: &str = "LABEL.METADATA_MAX_ATTEMPTS_RESOLVE";
const LABEL_MAX_ATTEMPTS_PROBE: &str = "LABEL.METADATA_MAX_ATTEMPTS_PROBE";
const LABEL_BACKOFF_JITTER_PERCENT: &str = "LABEL.METADATA_BACKOFF_JITTER_PERCENT";
const LABEL_MAX_QUEUE_SIZE: &str = "LABEL.METADATA_MAX_QUEUE_SIZE";
const LABEL_FFPROBE_ENABLED: &str = "LABEL.FFPROBE_ENABLED";
const LABEL_FFPROBE_TIMEOUT: &str = "LABEL.FFPROBE_TIMEOUT";
const LABEL_FFPROBE_ANALYZE_DURATION: &str = "LABEL.METADATA_FFPROBE_ANALYZE_DURATION";
const LABEL_FFPROBE_PROBE_SIZE: &str = "LABEL.METADATA_FFPROBE_PROBE_SIZE";
const LABEL_FFPROBE_LIVE_ANALYZE_DURATION: &str = "LABEL.METADATA_FFPROBE_LIVE_ANALYZE_DURATION";
const LABEL_FFPROBE_LIVE_PROBE_SIZE: &str = "LABEL.METADATA_FFPROBE_LIVE_PROBE_SIZE";
const LABEL_TMDB: &str = "LABEL.TMDB";
const LABEL_LOG: &str = "LABEL.LOG";
const LABEL_RESOLVE: &str = "LABEL.RESOLVE";
const LABEL_PROBE: &str = "LABEL.PROBE";
const LABEL_FFPROBE: &str = "LABEL.FFPROBE";
const LABEL_SETTINGS: &str = "LABEL.SETTINGS";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_API_KEY: &str = "LABEL.API_KEY";
const LABEL_RATE_LIMIT_MS: &str = "LABEL.RATE_LIMIT_MS";
const LABEL_CACHE_DURATION_DAYS: &str = "LABEL.CACHE_DURATION_DAYS";
const LABEL_LANGUAGE: &str = "LABEL.LANGUAGE";

generate_form_reducer!(
    state: MetadataUpdateConfigFormState { form: MetadataUpdateConfigDto },
    action_name: MetadataUpdateConfigFormAction,
    fields {
        RetryDelay => retry_delay: String,
        MaxQueueSize => max_queue_size: usize,
        WorkerIdleTimeout => worker_idle_timeout: String,
    }
);

generate_form_reducer!(
    state: MetadataLogConfigFormState { form: MetadataLogConfigDto },
    action_name: MetadataLogConfigFormAction,
    fields {
        QueueInterval => queue_interval: String,
        ProgressInterval => progress_interval: String,
    }
);

generate_form_reducer!(
    state: ResolveConfigFormState { form: ResolveConfigDto },
    action_name: ResolveConfigFormAction,
    fields {
        MaxRetryBackoff => max_retry_backoff: String,
        MinRetryBase => min_retry_base: String,
        ExhaustionResetGap => exhaustion_reset_gap: String,
        MaxAttempts => max_attempts: u8,
    }
);

generate_form_reducer!(
    state: ProbeConfigFormState { form: ProbeConfigDto },
    action_name: ProbeConfigFormAction,
    fields {
        Cooldown => cooldown: String,
        RetryLoadRetryDelay => retry_load_retry_delay: String,
        RetryBackoffStep1 => retry_backoff_step_1: String,
        RetryBackoffStep2 => retry_backoff_step_2: String,
        RetryBackoffStep3 => retry_backoff_step_3: String,
        MaxAttempts => max_attempts: u8,
        BackoffJitterPercent => backoff_jitter_percent: u8,
    }
);

generate_form_reducer!(
    state: FfprobeConfigFormState { form: FfprobeConfigDto },
    action_name: FfprobeConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Timeout => timeout: Option<u64>,
        AnalyzeDuration => analyze_duration: String,
        ProbeSize => probe_size: String,
        LiveAnalyzeDuration => live_analyze_duration: String,
        LiveProbeSize => live_probe_size: String,
    }
);

generate_form_reducer!(
    state: TmdbConfigFormState { form: TmdbConfigDto },
    action_name: TmdbConfigFormAction,
    fields {
        Enabled => enabled: bool,
        ApiKey => api_key: Option<String>,
        RateLimitMs => rate_limit_ms: u64,
        CacheDurationDays => cache_duration_days: u32,
        Language => language: String,
        Cooldown => cooldown: String,
    }
);

#[component]
pub fn MetadataUpdateConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");
    let backoff_jitter_error: UseStateHandle<Option<String>> = use_state(|| None);

    let form_state: UseReducerHandle<MetadataUpdateConfigFormState> =
        use_reducer(|| MetadataUpdateConfigFormState { form: MetadataUpdateConfigDto::default(), modified: false });
    let log_state: UseReducerHandle<MetadataLogConfigFormState> =
        use_reducer(|| MetadataLogConfigFormState { form: MetadataLogConfigDto::default(), modified: false });
    let resolve_state: UseReducerHandle<ResolveConfigFormState> =
        use_reducer(|| ResolveConfigFormState { form: ResolveConfigDto::default(), modified: false });
    let probe_state: UseReducerHandle<ProbeConfigFormState> =
        use_reducer(|| ProbeConfigFormState { form: ProbeConfigDto::default(), modified: false });
    let ffprobe_state: UseReducerHandle<FfprobeConfigFormState> =
        use_reducer(|| FfprobeConfigFormState { form: FfprobeConfigDto::default(), modified: false });
    let tmdb_state: UseReducerHandle<TmdbConfigFormState> =
        use_reducer(|| TmdbConfigFormState { form: TmdbConfigDto::default(), modified: false });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (
            form_state.clone(),
            log_state.clone(),
            resolve_state.clone(),
            probe_state.clone(),
            ffprobe_state.clone(),
            tmdb_state.clone(),
        );
        use_effect_with(deps, move |(form, log, resolve, probe, ffprobe, tmdb)| {
            let mut merged = form.form.clone();
            merged.log = log.form.clone();
            merged.resolve = resolve.form.clone();
            merged.probe = probe.form.clone();
            merged.ffprobe = ffprobe.form.clone();
            merged.tmdb = tmdb.form.clone();
            on_form_change.emit(ConfigForm::MetadataUpdate(
                form.modified
                    || log.modified
                    || resolve.modified
                    || probe.modified
                    || ffprobe.modified
                    || tmdb.modified,
                merged,
            ));
        });
    }

    {
        let form_state = form_state.clone();
        let log_state = log_state.clone();
        let resolve_state = resolve_state.clone();
        let probe_state = probe_state.clone();
        let ffprobe_state = ffprobe_state.clone();
        let tmdb_state = tmdb_state.clone();

        let metadata_update_cfg =
            config_ctx.config.as_ref().and_then(|c| c.config.metadata_update.clone()).unwrap_or_default();
        use_effect_with((metadata_update_cfg, config_view_ctx.edit_mode.clone()), move |(cfg, _mode)| {
            form_state.dispatch(MetadataUpdateConfigFormAction::SetAll(cfg.clone()));
            log_state.dispatch(MetadataLogConfigFormAction::SetAll(cfg.log.clone()));
            resolve_state.dispatch(ResolveConfigFormAction::SetAll(cfg.resolve.clone()));
            probe_state.dispatch(ProbeConfigFormAction::SetAll(cfg.probe.clone()));
            ffprobe_state.dispatch(FfprobeConfigFormAction::SetAll(cfg.ffprobe.clone()));
            tmdb_state.dispatch(TmdbConfigFormAction::SetAll(cfg.tmdb.clone()));
            || ()
        });
    }

    {
        let probe_state = probe_state.clone();
        let backoff_jitter_error = backoff_jitter_error.clone();
        use_effect_with(probe_state.form.backoff_jitter_percent, move |value| {
            if *value > 95 {
                backoff_jitter_error.set(Some("Backoff jitter percent must be between 0 and 95.".to_string()));
                probe_state.dispatch(ProbeConfigFormAction::BackoffJitterPercent(95));
            } else {
                backoff_jitter_error.set(None);
            }
            || ()
        });
    }

    let render_view_mode = || {
        let log = &log_state.form;
        let resolve = &resolve_state.form;
        let probe = &probe_state.form;
        let ffprobe = &ffprobe_state.form;
        let tmdb = &tmdb_state.form;

        html! {
            <>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_FFPROBE)}</h1>
                    { config_field_bool!(ffprobe, translate.t(LABEL_FFPROBE_ENABLED), enabled) }
                    { config_field_optional!(ffprobe, translate.t(LABEL_FFPROBE_TIMEOUT), timeout) }
                    { config_field!(ffprobe, translate.t(LABEL_FFPROBE_ANALYZE_DURATION), analyze_duration) }
                    { config_field!(ffprobe, translate.t(LABEL_FFPROBE_PROBE_SIZE), probe_size) }
                    { config_field!(ffprobe, translate.t(LABEL_FFPROBE_LIVE_ANALYZE_DURATION), live_analyze_duration) }
                    { config_field!(ffprobe, translate.t(LABEL_FFPROBE_LIVE_PROBE_SIZE), live_probe_size) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_TMDB)}</h1>
                    { config_field_bool!(tmdb, translate.t(LABEL_ENABLED), enabled) }
                    { config_field_optional!(tmdb, translate.t(LABEL_API_KEY), api_key) }
                    { config_field!(tmdb, translate.t(LABEL_RATE_LIMIT_MS), rate_limit_ms) }
                    { config_field!(tmdb, translate.t(LABEL_CACHE_DURATION_DAYS), cache_duration_days) }
                    { config_field!(tmdb, translate.t(LABEL_LANGUAGE), language) }
                    { config_field!(tmdb, translate.t(LABEL_TMDB_COOLDOWN), cooldown) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_SETTINGS)}</h1>
                    { config_field!(form_state.form, translate.t(LABEL_RETRY_DELAY), retry_delay) }
                    { config_field!(form_state.form, translate.t(LABEL_MAX_QUEUE_SIZE), max_queue_size) }
                    { config_field!(form_state.form, translate.t(LABEL_WORKER_IDLE_TIMEOUT), worker_idle_timeout) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_LOG)}</h1>
                    { config_field!(log, translate.t(LABEL_QUEUE_LOG_INTERVAL), queue_interval) }
                    { config_field!(log, translate.t(LABEL_PROGRESS_LOG_INTERVAL), progress_interval) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_RESOLVE)}</h1>
                    { config_field!(resolve, translate.t(LABEL_MAX_ATTEMPTS_RESOLVE), max_attempts) }
                    { config_field!(resolve, translate.t(LABEL_RESOLVE_MIN_RETRY_BASE), min_retry_base) }
                    { config_field!(resolve, translate.t(LABEL_MAX_RESOLVE_RETRY_BACKOFF), max_retry_backoff) }
                    { config_field!(resolve, translate.t(LABEL_RESOLVE_EXHAUSTION_RESET_GAP), exhaustion_reset_gap) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_PROBE)}</h1>
                    { config_field!(probe, translate.t(LABEL_MAX_ATTEMPTS_PROBE), max_attempts) }
                    { config_field!(probe, translate.t(LABEL_BACKOFF_JITTER_PERCENT), backoff_jitter_percent) }
                    { config_field!(probe, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_1), retry_backoff_step_1) }
                    { config_field!(probe, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_2), retry_backoff_step_2) }
                    { config_field!(probe, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_3), retry_backoff_step_3) }
                    { config_field!(probe, translate.t(LABEL_PROBE_RETRY_LOAD_RETRY_DELAY), retry_load_retry_delay) }
                    { config_field!(probe, translate.t(LABEL_PROBE_COOLDOWN), cooldown) }
                </Card>

            </>
        }
    };

    let render_edit_mode = || {
        let jitter_error_text = (*backoff_jitter_error).clone();
        let jitter_error_state = backoff_jitter_error.clone();
        let jitter_probe_state = probe_state.clone();
        let jitter_label = translate.t(LABEL_BACKOFF_JITTER_PERCENT);
        let jitter_field_id = dto_field_id(&jitter_probe_state.form, "backoff_jitter_percent");

        html! {
            <>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_FFPROBE)}</h1>
                    { edit_field_bool!(ffprobe_state, translate.t(LABEL_FFPROBE_ENABLED), enabled, FfprobeConfigFormAction::Enabled) }
                    { edit_field_number_option_u64!(ffprobe_state, translate.t(LABEL_FFPROBE_TIMEOUT), timeout, FfprobeConfigFormAction::Timeout) }
                    { edit_field_text!(ffprobe_state, translate.t(LABEL_FFPROBE_ANALYZE_DURATION), analyze_duration, FfprobeConfigFormAction::AnalyzeDuration) }
                    { edit_field_text!(ffprobe_state, translate.t(LABEL_FFPROBE_PROBE_SIZE), probe_size, FfprobeConfigFormAction::ProbeSize) }
                    { edit_field_text!(ffprobe_state, translate.t(LABEL_FFPROBE_LIVE_ANALYZE_DURATION), live_analyze_duration, FfprobeConfigFormAction::LiveAnalyzeDuration) }
                    { edit_field_text!(ffprobe_state, translate.t(LABEL_FFPROBE_LIVE_PROBE_SIZE), live_probe_size, FfprobeConfigFormAction::LiveProbeSize) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_TMDB)}</h1>
                    { edit_field_bool!(tmdb_state, translate.t(LABEL_ENABLED), enabled, TmdbConfigFormAction::Enabled) }
                    { edit_field_text_option!(tmdb_state, translate.t(LABEL_API_KEY), api_key, TmdbConfigFormAction::ApiKey, true) }
                    { edit_field_number_u64!(tmdb_state, translate.t(LABEL_RATE_LIMIT_MS), rate_limit_ms, TmdbConfigFormAction::RateLimitMs) }
                    { edit_field_number!(tmdb_state, translate.t(LABEL_CACHE_DURATION_DAYS), cache_duration_days, TmdbConfigFormAction::CacheDurationDays) }
                    { edit_field_text!(tmdb_state, translate.t(LABEL_LANGUAGE), language, TmdbConfigFormAction::Language) }
                    { edit_field_text!(tmdb_state, translate.t(LABEL_TMDB_COOLDOWN), cooldown, TmdbConfigFormAction::Cooldown) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_SETTINGS)}</h1>
                    { edit_field_text!(form_state, translate.t(LABEL_RETRY_DELAY), retry_delay, MetadataUpdateConfigFormAction::RetryDelay) }
                    { edit_field_number_usize!(form_state, translate.t(LABEL_MAX_QUEUE_SIZE), max_queue_size, MetadataUpdateConfigFormAction::MaxQueueSize) }
                    { edit_field_text!(form_state, translate.t(LABEL_WORKER_IDLE_TIMEOUT), worker_idle_timeout, MetadataUpdateConfigFormAction::WorkerIdleTimeout) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_LOG)}</h1>
                    { edit_field_text!(log_state, translate.t(LABEL_QUEUE_LOG_INTERVAL), queue_interval, MetadataLogConfigFormAction::QueueInterval) }
                    { edit_field_text!(log_state, translate.t(LABEL_PROGRESS_LOG_INTERVAL), progress_interval, MetadataLogConfigFormAction::ProgressInterval) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_RESOLVE)}</h1>
                    { edit_field_number_u8!(resolve_state, translate.t(LABEL_MAX_ATTEMPTS_RESOLVE), max_attempts, ResolveConfigFormAction::MaxAttempts) }
                    { edit_field_text!(resolve_state, translate.t(LABEL_RESOLVE_MIN_RETRY_BASE), min_retry_base, ResolveConfigFormAction::MinRetryBase) }
                    { edit_field_text!(resolve_state, translate.t(LABEL_MAX_RESOLVE_RETRY_BACKOFF), max_retry_backoff, ResolveConfigFormAction::MaxRetryBackoff) }
                    { edit_field_text!(resolve_state, translate.t(LABEL_RESOLVE_EXHAUSTION_RESET_GAP), exhaustion_reset_gap, ResolveConfigFormAction::ExhaustionResetGap) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_PROBE)}</h1>
                    { edit_field_number_u8!(probe_state, translate.t(LABEL_MAX_ATTEMPTS_PROBE), max_attempts, ProbeConfigFormAction::MaxAttempts) }
                    <div class="tp__form-field tp__form-field__number">
                        <NumberInput
                            label={Some(jitter_label)}
                            name={"backoff_jitter_percent"}
                            field_id={Some(jitter_field_id)}
                            value={Some(i64::from(jitter_probe_state.form.backoff_jitter_percent))}
                            on_change={Callback::from(move |value: Option<i64>| {
                                match value {
                                    Some(raw) if !(0..=95).contains(&raw) => {
                                        jitter_error_state.set(Some("Backoff jitter percent must be between 0 and 95.".to_string()));
                                    }
                                    Some(raw) => {
                                        jitter_error_state.set(None);
                                        if let Ok(parsed) = u8::try_from(raw) {
                                            jitter_probe_state.dispatch(ProbeConfigFormAction::BackoffJitterPercent(parsed));
                                        }
                                    }
                                    None => {
                                        jitter_error_state.set(None);
                                        jitter_probe_state.dispatch(ProbeConfigFormAction::BackoffJitterPercent(0));
                                    }
                                }
                            })}
                        />
                        {
                            if let Some(error_text) = jitter_error_text {
                                html! { <div class="tp__error-text">{error_text}</div> }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                    { edit_field_text!(probe_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_1), retry_backoff_step_1, ProbeConfigFormAction::RetryBackoffStep1) }
                    { edit_field_text!(probe_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_2), retry_backoff_step_2, ProbeConfigFormAction::RetryBackoffStep2) }
                    { edit_field_text!(probe_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_3), retry_backoff_step_3, ProbeConfigFormAction::RetryBackoffStep3) }
                    { edit_field_text!(probe_state, translate.t(LABEL_PROBE_RETRY_LOAD_RETRY_DELAY), retry_load_retry_delay, ProbeConfigFormAction::RetryLoadRetryDelay) }
                    { edit_field_text!(probe_state, translate.t(LABEL_PROBE_COOLDOWN), cooldown, ProbeConfigFormAction::Cooldown) }
                </Card>

            </>
        }
    };

    html! {
        <div class="tp__metadata-update-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_METADATA_UPDATE_CONFIG)}</div>
            <div class="tp__metadata-update-config-view__body tp__config-view-page__body">
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
