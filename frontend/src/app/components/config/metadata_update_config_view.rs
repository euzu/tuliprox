use crate::{
    app::{
        components::config::{
            config_page::{ConfigForm, LABEL_METADATA_UPDATE_CONFIG},
            config_view_context::ConfigViewContext,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_optional, edit_field_bool, edit_field_number_option_u64,
    edit_field_number_u8, edit_field_number_usize, edit_field_text, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::MetadataUpdateConfigDto;
use yew::prelude::*;

const LABEL_QUEUE_LOG_INTERVAL: &str = "LABEL.METADATA_QUEUE_LOG_INTERVAL";
const LABEL_PROGRESS_LOG_INTERVAL: &str = "LABEL.METADATA_PROGRESS_LOG_INTERVAL";
const LABEL_MAX_RESOLVE_RETRY_BACKOFF: &str = "LABEL.METADATA_MAX_RESOLVE_RETRY_BACKOFF";
const LABEL_RESOLVE_MIN_RETRY_BASE: &str = "LABEL.METADATA_RESOLVE_MIN_RETRY_BASE";
const LABEL_RESOLVE_EXHAUSTION_RESET_GAP: &str = "LABEL.METADATA_RESOLVE_EXHAUSTION_RESET_GAP";
const LABEL_PROBE_COOLDOWN: &str = "LABEL.METADATA_PROBE_COOLDOWN";
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

generate_form_reducer!(
    state: MetadataUpdateConfigFormState { form: MetadataUpdateConfigDto },
    action_name: MetadataUpdateConfigFormAction,
    fields {
        FfprobeEnabled => ffprobe_enabled: bool,
        FfprobeTimeout => ffprobe_timeout: Option<u64>,
        FfprobeAnalyzeDuration => ffprobe_analyze_duration: String,
        FfprobeProbeSize => ffprobe_probe_size: String,
        FfprobeLiveAnalyzeDuration => ffprobe_live_analyze_duration: String,
        FfprobeLiveProbeSize => ffprobe_live_probe_size: String,
        MaxAttemptsResolve => max_attempts_resolve: u8,
        MaxAttemptsProbe => max_attempts_probe: u8,
        BackoffJitterPercent => backoff_jitter_percent: u8,
        ResolveMinRetryBase => resolve_min_retry_base: String,
        MaxResolveRetryBackoff => max_resolve_retry_backoff: String,
        ProbeRetryBackoffStep1 => probe_retry_backoff_step_1: String,
        ProbeRetryBackoffStep2 => probe_retry_backoff_step_2: String,
        ProbeRetryBackoffStep3 => probe_retry_backoff_step_3: String,
        RetryDelay => retry_delay: String,
        ProbeRetryLoadRetryDelay => probe_retry_load_retry_delay: String,
        ResolveExhaustionResetGap => resolve_exhaustion_reset_gap: String,
        ProbeCooldown => probe_cooldown: String,
        MaxQueueSize => max_queue_size: usize,
        QueueLogInterval => queue_log_interval: String,
        ProgressLogInterval => progress_log_interval: String,
        WorkerIdleTimeout => worker_idle_timeout: String,
    }
);

#[component]
pub fn MetadataUpdateConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<MetadataUpdateConfigFormState> =
        use_reducer(|| MetadataUpdateConfigFormState { form: MetadataUpdateConfigDto::default(), modified: false });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::MetadataUpdate(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let metadata_update_cfg =
            config_ctx.config.as_ref().and_then(|c| c.config.metadata_update.clone()).unwrap_or_default();
        use_effect_with((metadata_update_cfg, config_view_ctx.edit_mode.clone()), move |(cfg, _mode)| {
            form_state.dispatch(MetadataUpdateConfigFormAction::SetAll(cfg.clone()));
            || ()
        });
    }

    let render_view_mode = || {
        html! {
            <>
                { config_field_bool!(form_state.form, translate.t(LABEL_FFPROBE_ENABLED), ffprobe_enabled) }
                { config_field_optional!(form_state.form, translate.t(LABEL_FFPROBE_TIMEOUT), ffprobe_timeout) }
                { config_field!(form_state.form, translate.t(LABEL_FFPROBE_ANALYZE_DURATION), ffprobe_analyze_duration) }
                { config_field!(form_state.form, translate.t(LABEL_FFPROBE_PROBE_SIZE), ffprobe_probe_size) }
                { config_field!(form_state.form, translate.t(LABEL_FFPROBE_LIVE_ANALYZE_DURATION), ffprobe_live_analyze_duration) }
                { config_field!(form_state.form, translate.t(LABEL_FFPROBE_LIVE_PROBE_SIZE), ffprobe_live_probe_size) }
                { config_field!(form_state.form, translate.t(LABEL_MAX_ATTEMPTS_RESOLVE), max_attempts_resolve) }
                { config_field!(form_state.form, translate.t(LABEL_MAX_ATTEMPTS_PROBE), max_attempts_probe) }
                { config_field!(form_state.form, translate.t(LABEL_BACKOFF_JITTER_PERCENT), backoff_jitter_percent) }
                { config_field!(form_state.form, translate.t(LABEL_RESOLVE_MIN_RETRY_BASE), resolve_min_retry_base) }
                { config_field!(form_state.form, translate.t(LABEL_MAX_RESOLVE_RETRY_BACKOFF), max_resolve_retry_backoff) }
                { config_field!(form_state.form, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_1), probe_retry_backoff_step_1) }
                { config_field!(form_state.form, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_2), probe_retry_backoff_step_2) }
                { config_field!(form_state.form, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_3), probe_retry_backoff_step_3) }
                { config_field!(form_state.form, translate.t(LABEL_RETRY_DELAY), retry_delay) }
                { config_field!(form_state.form, translate.t(LABEL_PROBE_RETRY_LOAD_RETRY_DELAY), probe_retry_load_retry_delay) }
                { config_field!(form_state.form, translate.t(LABEL_RESOLVE_EXHAUSTION_RESET_GAP), resolve_exhaustion_reset_gap) }
                { config_field!(form_state.form, translate.t(LABEL_PROBE_COOLDOWN), probe_cooldown) }
                { config_field!(form_state.form, translate.t(LABEL_MAX_QUEUE_SIZE), max_queue_size) }
                { config_field!(form_state.form, translate.t(LABEL_QUEUE_LOG_INTERVAL), queue_log_interval) }
                { config_field!(form_state.form, translate.t(LABEL_PROGRESS_LOG_INTERVAL), progress_log_interval) }
                { config_field!(form_state.form, translate.t(LABEL_WORKER_IDLE_TIMEOUT), worker_idle_timeout) }
            </>
        }
    };

    let render_edit_mode = || {
        html! {
            <>
                { edit_field_bool!(form_state, translate.t(LABEL_FFPROBE_ENABLED), ffprobe_enabled, MetadataUpdateConfigFormAction::FfprobeEnabled) }
                { edit_field_number_option_u64!(form_state, translate.t(LABEL_FFPROBE_TIMEOUT), ffprobe_timeout, MetadataUpdateConfigFormAction::FfprobeTimeout) }
                { edit_field_text!(form_state, translate.t(LABEL_FFPROBE_ANALYZE_DURATION), ffprobe_analyze_duration, MetadataUpdateConfigFormAction::FfprobeAnalyzeDuration) }
                { edit_field_text!(form_state, translate.t(LABEL_FFPROBE_PROBE_SIZE), ffprobe_probe_size, MetadataUpdateConfigFormAction::FfprobeProbeSize) }
                { edit_field_text!(form_state, translate.t(LABEL_FFPROBE_LIVE_ANALYZE_DURATION), ffprobe_live_analyze_duration, MetadataUpdateConfigFormAction::FfprobeLiveAnalyzeDuration) }
                { edit_field_text!(form_state, translate.t(LABEL_FFPROBE_LIVE_PROBE_SIZE), ffprobe_live_probe_size, MetadataUpdateConfigFormAction::FfprobeLiveProbeSize) }
                { edit_field_number_u8!(form_state, translate.t(LABEL_MAX_ATTEMPTS_RESOLVE), max_attempts_resolve, MetadataUpdateConfigFormAction::MaxAttemptsResolve) }
                { edit_field_number_u8!(form_state, translate.t(LABEL_MAX_ATTEMPTS_PROBE), max_attempts_probe, MetadataUpdateConfigFormAction::MaxAttemptsProbe) }
                { edit_field_number_u8!(form_state, translate.t(LABEL_BACKOFF_JITTER_PERCENT), backoff_jitter_percent, MetadataUpdateConfigFormAction::BackoffJitterPercent) }
                { edit_field_text!(form_state, translate.t(LABEL_RESOLVE_MIN_RETRY_BASE), resolve_min_retry_base, MetadataUpdateConfigFormAction::ResolveMinRetryBase) }
                { edit_field_text!(form_state, translate.t(LABEL_MAX_RESOLVE_RETRY_BACKOFF), max_resolve_retry_backoff, MetadataUpdateConfigFormAction::MaxResolveRetryBackoff) }
                { edit_field_text!(form_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_1), probe_retry_backoff_step_1, MetadataUpdateConfigFormAction::ProbeRetryBackoffStep1) }
                { edit_field_text!(form_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_2), probe_retry_backoff_step_2, MetadataUpdateConfigFormAction::ProbeRetryBackoffStep2) }
                { edit_field_text!(form_state, translate.t(LABEL_PROBE_RETRY_BACKOFF_STEP_3), probe_retry_backoff_step_3, MetadataUpdateConfigFormAction::ProbeRetryBackoffStep3) }
                { edit_field_text!(form_state, translate.t(LABEL_RETRY_DELAY), retry_delay, MetadataUpdateConfigFormAction::RetryDelay) }
                { edit_field_text!(form_state, translate.t(LABEL_PROBE_RETRY_LOAD_RETRY_DELAY), probe_retry_load_retry_delay, MetadataUpdateConfigFormAction::ProbeRetryLoadRetryDelay) }
                { edit_field_text!(form_state, translate.t(LABEL_RESOLVE_EXHAUSTION_RESET_GAP), resolve_exhaustion_reset_gap, MetadataUpdateConfigFormAction::ResolveExhaustionResetGap) }
                { edit_field_text!(form_state, translate.t(LABEL_PROBE_COOLDOWN), probe_cooldown, MetadataUpdateConfigFormAction::ProbeCooldown) }
                { edit_field_number_usize!(form_state, translate.t(LABEL_MAX_QUEUE_SIZE), max_queue_size, MetadataUpdateConfigFormAction::MaxQueueSize) }
                { edit_field_text!(form_state, translate.t(LABEL_QUEUE_LOG_INTERVAL), queue_log_interval, MetadataUpdateConfigFormAction::QueueLogInterval) }
                { edit_field_text!(form_state, translate.t(LABEL_PROGRESS_LOG_INTERVAL), progress_log_interval, MetadataUpdateConfigFormAction::ProgressLogInterval) }
                { edit_field_text!(form_state, translate.t(LABEL_WORKER_IDLE_TIMEOUT), worker_idle_timeout, MetadataUpdateConfigFormAction::WorkerIdleTimeout) }
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
