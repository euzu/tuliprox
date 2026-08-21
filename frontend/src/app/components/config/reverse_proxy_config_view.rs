#![allow(clippy::large_enum_variant)]

use crate::{
    app::{
        components::{
            config::{
                add_admission_strategy_tag, admission_strategy_label, admission_strategy_tag_label,
                admission_strategy_tags, available_admission_strategies,
                config_page::{ConfigForm, LABEL_REVERSE_PROXY_CONFIG},
                config_view_context::ConfigViewContext,
                displayed_admission_strategy_tags, filter_disabled_grace_strategies, move_admission_strategy_tag,
                parse_admission_strategy_tags, remove_admission_strategy_tag, use_emit_mapped_option,
                AdmissionStrategiesDto,
            },
            dto_field_id,
            number_input::NumberInput,
            Card, Chip, DropDownOption, DropDownSelection, IconButton, RadioButtonGroup, Select, TextButton,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_hide, config_field_optional,
    edit_field_bool, edit_field_byte_size_option, edit_field_list, edit_field_number, edit_field_number_f64,
    edit_field_number_u16, edit_field_number_u64, edit_field_number_usize, edit_field_text, edit_field_text_option,
    generate_form_reducer,
    i18n::{use_translation, YewI18n},
};
use shared::{
    defaults::default_secret,
    model::{
        ByteSize, CacheConfigDto, GeoIpConfigDto, GeoIpUnavailablePolicy, HlsCacheConfigDto,
        HlsCorruptSegmentWatchdogMode, HlsManifestRecoveryBurstConfigDto, HlsManifestRecoveryBurstLevel,
        HlsSegmentRepairConfigDto, HlsSegmentRepairMode, HlsStripConfigDto, HlsStripMode, QosAggregationConfigDto,
        RateLimitConfigDto, ResourceRetryConfigDto, ReverseProxyConfigDto, ReverseProxyDisabledHeaderConfigDto,
        StreamBufferConfigDto, StreamConfigDto, StreamHistoryConfigDto,
    },
    utils::format_float_localized,
};
use std::{rc::Rc, str::FromStr};
use strum::IntoEnumIterator;
use web_sys::HtmlInputElement;
use yew::prelude::*;

const LABEL_CACHE: &str = "LABEL.CACHE";
const LABEL_RESOURCE_IMAGE_CACHE: &str = "LABEL.RESOURCE_IMAGE_CACHE";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_SIZE: &str = "LABEL.SIZE";
const LABEL_DIRECTORY: &str = "LABEL.DIRECTORY";
const LABEL_STREAM: &str = "LABEL.STREAM";
const LABEL_STREAM_GRACE: &str = "LABEL.STREAM_GRACE";
const LABEL_STREAM_SESSION: &str = "LABEL.STREAM_SESSION";
const LABEL_STREAM_METRICS_ENABLED: &str = "LABEL.STREAM_METRICS_ENABLED";
const LABEL_RETRY: &str = "LABEL.RETRY";
const LABEL_THROTTLE: &str = "LABEL.THROTTLE";
const LABEL_GRACE_PERIOD_MILLIS: &str = "LABEL.GRACE_PERIOD_MILLIS";
const LABEL_GRACE_PERIOD_TIMEOUT_SECS: &str = "LABEL.GRACE_PERIOD_TIMEOUT_SECS";
const LABEL_GRACE_PERIOD_HOLD_STREAM: &str = "LABEL.GRACE_PERIOD_HOLD_STREAM";
const LABEL_HLS_SESSION_TTL_SECS: &str = "LABEL.HLS_SESSION_TTL_SECS";
const LABEL_CATCHUP_SESSION_TTL_SECS: &str = "LABEL.CATCHUP_SESSION_TTL_SECS";
const LABEL_THROTTLE_KBPS: &str = "LABEL.THROTTLE_KBPS";
const LABEL_STREAM_BUFFER: &str = "LABEL.STREAM_BUFFER";
const LABEL_BUFFER_ENABLED: &str = "LABEL.BUFFER_ENABLED";
const LABEL_BUFFER_SIZE: &str = "LABEL.BUFFER_SIZE";
const LABEL_HLS_CACHE_PROXY: &str = "LABEL.HLS_CACHE_PROXY";
const LABEL_HLS_CACHE_SEGMENT_REPAIR: &str = "LABEL.HLS_CACHE_SEGMENT_REPAIR";
const LABEL_CACHE_PATH: &str = "LABEL.CACHE_PATH";
const LABEL_STRIP_MODE: &str = "LABEL.STRIP_MODE";
const LABEL_STRIP_VALUE: &str = "LABEL.STRIP_VALUE";
const LABEL_CACHE_DURATION: &str = "LABEL.CACHE_DURATION";
const LABEL_CACHE_BYTES: &str = "LABEL.CACHE_BYTES";
const LABEL_CACHE_BYTES_PER_SESSION: &str = "LABEL.CACHE_BYTES_PER_SESSION";
const LABEL_MAX_SEGMENTS_PREFETCH: &str = "LABEL.MAX_SEGMENTS_PREFETCH";
const LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_PER_SESSION: &str = "LABEL.MAX_CONCURRENT_SEGMENT_FETCHES_PER_SESSION";
const LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_GLOBAL: &str = "LABEL.MAX_CONCURRENT_SEGMENT_FETCHES_GLOBAL";
const LABEL_ORIGIN_MANIFEST_TIMEOUT_MS: &str = "LABEL.ORIGIN_MANIFEST_TIMEOUT_MS";
const LABEL_MANIFEST_RECOVERY_BURST: &str = "LABEL.MANIFEST_RECOVERY_BURST";
const LABEL_ORIGIN_SEGMENT_TIMEOUT_MS: &str = "LABEL.ORIGIN_SEGMENT_TIMEOUT_MS";
const LABEL_SESSION_IDLE_TIMEOUT: &str = "LABEL.SESSION_IDLE_TIMEOUT";
const LABEL_SEGMENT_REPAIR: &str = "LABEL.SEGMENT_REPAIR";
const LABEL_SEGMENT_SIZE_INCREASE: &str = "LABEL.SEGMENT_SIZE_INCREASE";
const LABEL_REPAIR_TRIGGER: &str = "LABEL.REPAIR_TRIGGER";
const LABEL_APPLY_TO_FIRST_SEGMENTS: &str = "LABEL.APPLY_TO_FIRST_SEGMENTS";
const LABEL_MAX_PARALLEL_REPAIRS: &str = "LABEL.MAX_PARALLEL_REPAIRS";
const LABEL_POSTPROCESS_TIMEOUT_MS: &str = "LABEL.POSTPROCESS_TIMEOUT_MS";
const LABEL_CORRUPT_SEGMENT_WATCHDOG: &str = "LABEL.CORRUPT_SEGMENT_WATCHDOG";
const LABEL_MAX_WATCHDOG_JOBS: &str = "LABEL.MAX_WATCHDOG_JOBS";

const LABEL_RATE_LIMIT: &str = "LABEL.RATE_LIMIT";
const LABEL_PERIOD_MILLIS: &str = "LABEL.PERIOD_MILLIS";
const LABEL_BURST_SIZE: &str = "LABEL.BURST_SIZE";
const LABEL_SHARED_BURST_BUFFER_MB: &str = "LABEL.SHARED_BURST_BUFFER_BYTES";

const LABEL_ADMISSION_STRATEGIES: &str = "LABEL.ADMISSION_STRATEGIES";

const LABEL_SETTINGS: &str = "LABEL.SETTINGS";
const LABEL_RESOURCE_REWRITE_DISABLED: &str = "LABEL.RESOURCE_REWRITE_DISABLED";
const LABEL_REWRITE_SECRET: &str = "LABEL.REWRITE_SECRET";
const LABEL_RESOURCE_RETRY: &str = "LABEL.RESOURCE_RETRY";
const LABEL_MAX_ATTEMPTS: &str = "LABEL.MAX_ATTEMPTS";
const LABEL_BACKOFF_MILLIS: &str = "LABEL.BACKOFF_MILLIS";
const LABEL_BACKOFF_MULTIPLIER: &str = "LABEL.BACKOFF_MULTIPLIER";
const LABEL_FAILOVER_REDIRECT_PATTERNS: &str = "LABEL.FAILOVER_REDIRECT_PATTERNS";
const LABEL_ADD_PATTERN: &str = "LABEL.ADD_PATTERN";
const LABEL_DISABLED_HEADER: &str = "LABEL.DISABLED_HEADER";
const LABEL_REFERER_HEADER: &str = "LABEL.REFERER_HEADER";
const LABEL_X_HEADER: &str = "LABEL.X_HEADER";
const LABEL_CF_HEADER: &str = "LABEL.CF_HEADER";
const LABEL_CUSTOM_HEADERS: &str = "LABEL.CUSTOM_HEADERS";
const LABEL_ADD_HEADER: &str = "LABEL.ADD_HEADER";
const LABEL_GEOIP: &str = "LABEL.GEOIP";
const LABEL_GEOIP_UNAVAILABLE_POLICY: &str = "LABEL.GEOIP_UNAVAILABLE_POLICY";
const LABEL_URL: &str = "LABEL.URL";

const LABEL_STREAM_HISTORY: &str = "LABEL.STREAM_HISTORY";
const LABEL_STREAM_HISTORY_ENABLED: &str = "LABEL.STREAM_HISTORY_ENABLED";
const LABEL_STREAM_HISTORY_BATCH_SIZE: &str = "LABEL.STREAM_HISTORY_BATCH_SIZE";
const LABEL_STREAM_HISTORY_RETENTION_DAYS: &str = "LABEL.STREAM_HISTORY_RETENTION_DAYS";
const LABEL_QOS_AGGREGATION: &str = "LABEL.QOS_AGGREGATION";
const LABEL_QOS_AGGREGATION_ENABLED: &str = "LABEL.QOS_AGGREGATION_ENABLED";
const LABEL_INTERVAL_SECS: &str = "LABEL.INTERVAL_SECS";

generate_form_reducer!(
    state: CacheConfigFormState { form: CacheConfigDto },
    action_name: CacheConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Size => size: Option<ByteSize>,
        Dir => directory: Option<String>,
    }
);

generate_form_reducer!(
    state: RateLimitConfigFormState { form: RateLimitConfigDto },
    action_name: RateLimitConfigFormAction,
    fields {
        Enabled => enabled: bool,
        PeriodMillis => period_millis: u64,
        BurstSize => burst_size: u32,
    }
);

generate_form_reducer!(
    state: ResourceRetryConfigFormState { form: ResourceRetryConfigDto },
    action_name: ResourceRetryConfigFormAction,
    fields {
        MaxAttempts => max_attempts: u32,
        BackoffMillis => backoff_millis: u64,
        BackoffMultiplier => backoff_multiplier: f64,
    }
);

/// Simple wrapper for failover patterns to use Vec<String> directly with edit_field_list
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FailoverPatternsDto {
    pub patterns: Vec<String>,
}

impl FailoverPatternsDto {
    pub fn is_empty(&self) -> bool { self.patterns.is_empty() }
}

generate_form_reducer!(
    state: FailoverPatternsFormState { form: FailoverPatternsDto },
    action_name: FailoverPatternsFormAction,
    fields {
        Patterns => patterns: Vec<String>,
    }
);

generate_form_reducer!(
    state: AdmissionStrategiesFormState { form: AdmissionStrategiesDto },
    action_name: AdmissionStrategiesFormAction,
    fields {
        Strategies => strategies: Option<Vec<String>>,
    }
);

generate_form_reducer!(
    state: StreamConfigFormState { form: StreamConfigDto },
    action_name: StreamConfigFormAction,
    fields {
        Retry => retry: bool,
        MetricsEnabled => metrics_enabled: bool,
        Throttle => throttle: Option<String>,
        GracePeriodMillis => grace_period_millis: u64,
        GracePeriodTimeoutSecs => grace_period_timeout_secs: u64,
        ThrottleKbps => throttle_kbps: u64,
        SharedBurstBufferMb => shared_burst_buffer_mb: u64,
        GracePeriodHoldStream => grace_period_hold_stream: bool,
        HlsSessionTtlSecs => hls_session_ttl_secs: u64,
        CatchupSessionTtlSecs => catchup_session_ttl_secs: u64,
    }
);

generate_form_reducer!(
    state: StreamBufferConfigFormState { form: StreamBufferConfigDto },
    action_name: StreamBufferConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Size => size: usize,
    }
);

generate_form_reducer!(
    state: GeoIpConfigFormState { form: GeoIpConfigDto },
    action_name: GeoIpConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Url => url: String,
        UnavailablePolicy => unavailable_policy: GeoIpUnavailablePolicy,
    }
);

generate_form_reducer!(
    state: StripConfigFormState { form: HlsStripConfigDto },
    action_name: StripConfigFormAction,
    fields {
        Mode => mode: HlsStripMode,
        Value => value: u64,
    }
);

generate_form_reducer!(
    state: HlsCacheConfigFormState { form: HlsCacheConfigDto },
    action_name: HlsCacheConfigFormAction,
    fields {
        CachePath => cache_path: Option<String>,
        CacheDuration => cache_duration: u64,
        CacheBytes => cache_bytes: ByteSize,
        CacheBytesPerSession => cache_bytes_per_session: ByteSize,
        MaxSegmentsPrefetch => max_segments_prefetch: usize,
        MaxConcurrentSegmentFetchesPerSession => max_concurrent_segment_fetches_per_session: usize,
        MaxConcurrentSegmentFetchesGlobal => max_concurrent_segment_fetches_global: usize,
        OriginManifestTimeoutMs => origin_manifest_timeout_ms: u64,
        ManifestRecoveryBurst => manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto,
        OriginSegmentTimeoutMs => origin_segment_timeout_ms: u64,
        SessionIdleTimeout => session_idle_timeout: u64,
        SegmentRepair => segment_repair: HlsSegmentRepairConfigDto,
    }
);

generate_form_reducer!(
    state: StreamHistoryConfigFormState { form: StreamHistoryConfigDto },
    action_name: StreamHistoryConfigFormAction,
    fields {
        Enabled => stream_history_enabled: bool,
        BatchSize => stream_history_batch_size: usize,
        RetentionDays => stream_history_retention_days: u16,
        Directory => stream_history_directory: String,
    }
);

generate_form_reducer!(
    state: QosAggregationConfigFormState { form: QosAggregationConfigDto },
    action_name: QosAggregationConfigFormAction,
    fields {
        Enabled => enabled: bool,
        IntervalSecs => interval_secs: u64,
    }
);

generate_form_reducer!(
    state: ReverseProxyConfigFormState { form: ReverseProxyConfigDto },
    action_name: ReverseProxyConfigFormAction,
    fields {
        ResourceRewriteDisabled => resource_rewrite_disabled: bool,
        RewriteSecret => rewrite_secret: String,
    }
);

generate_form_reducer!(
    state: ReverseProxyDisabledHeaderConfigFormState { form: ReverseProxyDisabledHeaderConfigDto },
    action_name: ReverseProxyDisabledHeaderConfigFormAction,
    fields {
        RefererHeader => referer_header: bool,
        XHeader => x_header: bool,
        CloudflareHeader => cloudflare_header: bool,
        CustomHeader => custom_header: Vec<String>,
    }
);

fn geoip_unavailable_policy_options() -> Rc<Vec<String>> {
    Rc::new(GeoIpUnavailablePolicy::iter().map(|policy| policy.to_string()).collect())
}

pub(crate) fn geoip_unavailable_policy_label(translate: &YewI18n, policy: GeoIpUnavailablePolicy) -> String {
    match policy {
        GeoIpUnavailablePolicy::Deny => translate.t("LABEL.GEOIP_UNAVAILABLE_POLICY_DENY"),
        GeoIpUnavailablePolicy::Allow => translate.t("LABEL.GEOIP_UNAVAILABLE_POLICY_ALLOW"),
    }
}

fn geoip_unavailable_policy_labels(translate: &YewI18n) -> Rc<Vec<String>> {
    Rc::new(vec![
        geoip_unavailable_policy_label(translate, GeoIpUnavailablePolicy::Deny),
        geoip_unavailable_policy_label(translate, GeoIpUnavailablePolicy::Allow),
    ])
}

fn hls_strip_mode_options(selected: HlsStripMode) -> Rc<Vec<DropDownOption>> {
    Rc::new(vec![
        DropDownOption::new("segments", html! { "segments" }, selected == HlsStripMode::Segments),
        DropDownOption::new("seconds", html! { "seconds" }, selected == HlsStripMode::Seconds),
    ])
}

fn hls_manifest_recovery_burst_options(selected: HlsManifestRecoveryBurstLevel) -> Rc<Vec<DropDownOption>> {
    Rc::new(vec![
        DropDownOption::new("off", html! { "OFF" }, selected == HlsManifestRecoveryBurstLevel::Off),
        DropDownOption::new("friendly", html! { "FRIENDLY" }, selected == HlsManifestRecoveryBurstLevel::Friendly),
        DropDownOption::new("cautious", html! { "CAUTIOUS" }, selected == HlsManifestRecoveryBurstLevel::Cautious),
        DropDownOption::new("balanced", html! { "BALANCED" }, selected == HlsManifestRecoveryBurstLevel::Balanced),
        DropDownOption::new("intense", html! { "INTENSE" }, selected == HlsManifestRecoveryBurstLevel::Intense),
        DropDownOption::new(
            "aggressive",
            html! { "AGGRESSIVE" },
            selected == HlsManifestRecoveryBurstLevel::Aggressive,
        ),
        DropDownOption::new("beast", html! { "BEAST" }, selected == HlsManifestRecoveryBurstLevel::Beast),
    ])
}

fn hls_manifest_recovery_burst_label(level: HlsManifestRecoveryBurstLevel) -> &'static str {
    match level {
        HlsManifestRecoveryBurstLevel::Off => "OFF",
        HlsManifestRecoveryBurstLevel::Friendly => "FRIENDLY",
        HlsManifestRecoveryBurstLevel::Cautious => "CAUTIOUS",
        HlsManifestRecoveryBurstLevel::Balanced => "BALANCED",
        HlsManifestRecoveryBurstLevel::Intense => "INTENSE",
        HlsManifestRecoveryBurstLevel::Aggressive => "AGGRESSIVE",
        HlsManifestRecoveryBurstLevel::Beast => "BEAST",
    }
}

fn hls_segment_repair_max_level_options(selected: HlsSegmentRepairMode) -> Rc<Vec<DropDownOption>> {
    Rc::new(vec![
        DropDownOption::new("off", html! { "OFF" }, selected == HlsSegmentRepairMode::Off),
        DropDownOption::new("low", html! { "LOW" }, selected == HlsSegmentRepairMode::Low),
        DropDownOption::new("medium", html! { "MEDIUM" }, selected == HlsSegmentRepairMode::Medium),
        DropDownOption::new("high", html! { "HIGH" }, selected == HlsSegmentRepairMode::High),
    ])
}

fn hls_segment_repair_max_level_label(mode: HlsSegmentRepairMode) -> &'static str {
    match mode {
        HlsSegmentRepairMode::Off => "OFF",
        HlsSegmentRepairMode::Low => "LOW",
        HlsSegmentRepairMode::Medium => "MEDIUM",
        HlsSegmentRepairMode::High => "HIGH",
    }
}

fn hls_corrupt_segment_watchdog_options(selected: HlsCorruptSegmentWatchdogMode) -> Rc<Vec<DropDownOption>> {
    Rc::new(vec![
        DropDownOption::new("off", html! { "OFF" }, selected == HlsCorruptSegmentWatchdogMode::Off),
        DropDownOption::new(
            "detect_only",
            html! { "DETECT ONLY" },
            selected == HlsCorruptSegmentWatchdogMode::DetectOnly,
        ),
        DropDownOption::new("sanitize", html! { "SANITIZE" }, selected == HlsCorruptSegmentWatchdogMode::Sanitize),
        DropDownOption::new(
            "diagnostic",
            html! { "DIAGNOSTIC" },
            selected == HlsCorruptSegmentWatchdogMode::Diagnostic,
        ),
    ])
}

fn hls_corrupt_segment_watchdog_label(mode: HlsCorruptSegmentWatchdogMode) -> &'static str {
    match mode {
        HlsCorruptSegmentWatchdogMode::Off => "OFF",
        HlsCorruptSegmentWatchdogMode::DetectOnly => "DETECT ONLY",
        HlsCorruptSegmentWatchdogMode::Sanitize => "SANITIZE",
        HlsCorruptSegmentWatchdogMode::Diagnostic => "DIAGNOSTIC",
    }
}

fn hls_segment_repair_size_increase_percent(segment_repair: &HlsSegmentRepairConfigDto) -> Option<u8> {
    match segment_repair.max_level {
        HlsSegmentRepairMode::Off => None,
        HlsSegmentRepairMode::Low => Some(segment_repair.size_increase.low_percent),
        HlsSegmentRepairMode::Medium => Some(segment_repair.size_increase.medium_percent),
        HlsSegmentRepairMode::High => Some(segment_repair.size_increase.high_percent),
    }
}

fn set_hls_segment_repair_size_increase_percent(segment_repair: &mut HlsSegmentRepairConfigDto, value: u8) {
    match segment_repair.max_level {
        HlsSegmentRepairMode::Off => {}
        HlsSegmentRepairMode::Low => segment_repair.size_increase.low_percent = value,
        HlsSegmentRepairMode::Medium => segment_repair.size_increase.medium_percent = value,
        HlsSegmentRepairMode::High => segment_repair.size_increase.high_percent = value,
    }
}

fn hls_segment_repair_size_increase_label(translate: &YewI18n, mode: HlsSegmentRepairMode) -> String {
    match mode {
        HlsSegmentRepairMode::Off => translate.t(LABEL_SEGMENT_SIZE_INCREASE),
        _ => format!("{} ({})", translate.t(LABEL_SEGMENT_SIZE_INCREASE), hls_segment_repair_max_level_label(mode)),
    }
}

fn clamp_u64_min(value: Option<i64>, min_value: u64) -> u64 {
    value.and_then(|value| u64::try_from(value).ok()).filter(|value| *value >= min_value).unwrap_or(min_value)
}

fn clamp_usize_min(value: Option<i64>, min_value: usize) -> usize {
    value.and_then(|value| usize::try_from(value).ok()).filter(|value| *value >= min_value).unwrap_or(min_value)
}

fn clamp_u8_range(value: Option<i64>, min_value: u8, max_value: u8) -> u8 {
    value.and_then(|value| u8::try_from(value).ok()).map(|value| value.clamp(min_value, max_value)).unwrap_or(min_value)
}

#[component]
pub fn ReverseProxyConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let reverse_proxy_state: UseReducerHandle<ReverseProxyConfigFormState> =
        use_reducer(|| ReverseProxyConfigFormState {
            form: ReverseProxyConfigDto { rewrite_secret: default_secret(), ..Default::default() },
            modified: false,
        });
    let disabled_header_state: UseReducerHandle<ReverseProxyDisabledHeaderConfigFormState> =
        use_reducer(|| ReverseProxyDisabledHeaderConfigFormState {
            form: ReverseProxyDisabledHeaderConfigDto::default(),
            modified: false,
        });
    let cache_state: UseReducerHandle<CacheConfigFormState> =
        use_reducer(|| CacheConfigFormState { form: CacheConfigDto::default(), modified: false });
    let rate_limit_state: UseReducerHandle<RateLimitConfigFormState> =
        use_reducer(|| RateLimitConfigFormState { form: RateLimitConfigDto::default(), modified: false });
    let resource_retry_state: UseReducerHandle<ResourceRetryConfigFormState> =
        use_reducer(|| ResourceRetryConfigFormState { form: ResourceRetryConfigDto::default(), modified: false });
    let stream_state: UseReducerHandle<StreamConfigFormState> =
        use_reducer(|| StreamConfigFormState { form: StreamConfigDto::default(), modified: false });

    let geoip_state: UseReducerHandle<GeoIpConfigFormState> =
        use_reducer(|| GeoIpConfigFormState { form: GeoIpConfigDto::default(), modified: false });
    let hls_cache_state: UseReducerHandle<HlsCacheConfigFormState> =
        use_reducer(|| HlsCacheConfigFormState { form: HlsCacheConfigDto::default(), modified: false });
    let hls_strip_state: UseReducerHandle<StripConfigFormState> =
        use_reducer(|| StripConfigFormState { form: HlsStripConfigDto::default(), modified: false });

    let stream_buffer_state: UseReducerHandle<StreamBufferConfigFormState> =
        use_reducer(|| StreamBufferConfigFormState { form: StreamBufferConfigDto::default(), modified: false });

    let failover_patterns_state: UseReducerHandle<FailoverPatternsFormState> =
        use_reducer(|| FailoverPatternsFormState { form: FailoverPatternsDto::default(), modified: false });
    let admission_strategies_state: UseReducerHandle<AdmissionStrategiesFormState> =
        use_reducer(|| AdmissionStrategiesFormState { form: AdmissionStrategiesDto::default(), modified: false });
    let stream_history_state: UseReducerHandle<StreamHistoryConfigFormState> =
        use_reducer(|| StreamHistoryConfigFormState { form: StreamHistoryConfigDto::default(), modified: false });
    let qos_aggregation_state: UseReducerHandle<QosAggregationConfigFormState> =
        use_reducer(|| QosAggregationConfigFormState { form: QosAggregationConfigDto::default(), modified: false });
    let last_emitted_form = use_mut_ref(|| None::<ConfigForm>);

    {
        let reverse_proxy_state = reverse_proxy_state.clone();
        let disabled_header_state = disabled_header_state.clone();
        let cache_state = cache_state.clone();
        let rate_limit_state = rate_limit_state.clone();
        let resource_retry_state = resource_retry_state.clone();
        let stream_state = stream_state.clone();
        let geoip_state = geoip_state.clone();
        let hls_cache_state = hls_cache_state.clone();
        let hls_strip_state = hls_strip_state.clone();
        let stream_buffer_state = stream_buffer_state.clone();
        let failover_patterns_state = failover_patterns_state.clone();
        let admission_strategies_state = admission_strategies_state.clone();
        let stream_history_state = stream_history_state.clone();
        let qos_aggregation_state = qos_aggregation_state.clone();
        let last_emitted_form = last_emitted_form.clone();

        use_emit_mapped_option(
            (
                (
                    (
                        reverse_proxy_state.form.clone(),
                        disabled_header_state.form.clone(),
                        cache_state.form.clone(),
                        rate_limit_state.form.clone(),
                        resource_retry_state.form.clone(),
                        stream_state.form.clone(),
                        geoip_state.form.clone(),
                    ),
                    (
                        hls_cache_state.form.clone(),
                        hls_strip_state.form.clone(),
                        stream_buffer_state.form.clone(),
                        failover_patterns_state.form.clone(),
                        admission_strategies_state.form.clone(),
                        stream_history_state.form.clone(),
                        qos_aggregation_state.form.clone(),
                    ),
                ),
                (
                    (
                        reverse_proxy_state.modified,
                        disabled_header_state.modified,
                        cache_state.modified,
                        rate_limit_state.modified,
                        resource_retry_state.modified,
                        stream_state.modified,
                        geoip_state.modified,
                    ),
                    (
                        hls_cache_state.modified,
                        hls_strip_state.modified,
                        stream_buffer_state.modified,
                        failover_patterns_state.modified,
                        admission_strategies_state.modified,
                        stream_history_state.modified,
                        qos_aggregation_state.modified,
                    ),
                ),
            ),
            config_view_ctx.on_form_change.clone(),
            move |(
                (
                    (rp, disabled_header, cache, rl, resource_retry, stream, geoip),
                    (
                        hls_cache,
                        hls_strip,
                        stream_buffer,
                        failover_patterns,
                        admission_strategies,
                        stream_history,
                        qos_aggregation,
                    ),
                ),
                (
                    (
                        rp_modified,
                        disabled_header_modified,
                        cache_modified,
                        rl_modified,
                        resource_retry_modified,
                        stream_modified,
                        geoip_modified,
                    ),
                    (
                        hls_cache_modified,
                        hls_strip_modified,
                        stream_buffer_modified,
                        failover_patterns_modified,
                        admission_strategies_modified,
                        stream_history_modified,
                        qos_aggregation_modified,
                    ),
                ),
            )| {
                let mut form = rp.clone();
                let mut stream_form = stream.clone();
                stream_form.buffer = if stream_buffer.is_empty() { None } else { Some(stream_buffer.clone()) };
                stream_form.admission_strategies = filter_disabled_grace_strategies(
                    parse_admission_strategy_tags(admission_strategies.strategies.as_deref()),
                    stream_form.grace_period_millis,
                );

                form.cache = Some(cache.clone());
                form.rate_limit = Some(rl.clone());
                let mut resource_retry_form = resource_retry.clone();
                resource_retry_form.failover_redirect_patterns =
                    if failover_patterns.is_empty() { None } else { Some(failover_patterns.patterns.clone()) };
                form.resource_retry = Some(resource_retry_form);
                form.stream = Some(stream_form);
                form.geoip = Some(geoip.clone());
                let mut hls_cache_form = hls_cache.clone();
                hls_cache_form.strip = hls_strip.clone();
                form.hls_cache = Some(hls_cache_form);
                form.disabled_header = if disabled_header.is_empty() { None } else { Some(disabled_header.clone()) };
                form.stream_history = if stream_history.is_empty() { None } else { Some(stream_history.clone()) };
                form.qos_aggregation = if qos_aggregation.is_empty() { None } else { Some(qos_aggregation.clone()) };

                let modified = rp_modified
                    || disabled_header_modified
                    || cache_modified
                    || rl_modified
                    || resource_retry_modified
                    || stream_modified
                    || geoip_modified
                    || (hls_cache_modified || hls_strip_modified)
                    || stream_buffer_modified
                    || failover_patterns_modified
                    || admission_strategies_modified
                    || stream_history_modified
                    || qos_aggregation_modified;
                let next_form = ConfigForm::ReverseProxy(modified, form);
                let mut last_form = last_emitted_form.borrow_mut();
                if last_form.as_ref() != Some(&next_form) {
                    *last_form = Some(next_form);
                    last_form.clone()
                } else {
                    None
                }
            },
        );
    }

    {
        let reverse_proxy_state = reverse_proxy_state.clone();
        let disabled_header_state = disabled_header_state.clone();
        let cache_state = cache_state.clone();
        let rate_limit_state = rate_limit_state.clone();
        let resource_retry_state = resource_retry_state.clone();
        let stream_state = stream_state.clone();
        let geoip_state = geoip_state.clone();
        let hls_cache_state = hls_cache_state.clone();
        let hls_strip_state = hls_strip_state.clone();
        let stream_buffer_state = stream_buffer_state.clone();
        let failover_patterns_state = failover_patterns_state.clone();
        let admission_strategies_state = admission_strategies_state.clone();
        let stream_history_state = stream_history_state.clone();
        let qos_aggregation_state = qos_aggregation_state.clone();

        let reverse_proxy_cfg = config_ctx.config.as_ref().and_then(|c| c.config.reverse_proxy.clone());
        use_effect_with((reverse_proxy_cfg, *config_view_ctx.edit_mode), move |(cfg, _mode)| {
            if let Some(rp) = cfg {
                if reverse_proxy_state.form != *rp {
                    reverse_proxy_state.dispatch(ReverseProxyConfigFormAction::SetAll((*rp).clone()));
                }

                let target_disabled_header = rp
                    .disabled_header
                    .as_ref()
                    .map_or_else(ReverseProxyDisabledHeaderConfigDto::default, |d| d.clone());
                if disabled_header_state.form != target_disabled_header {
                    disabled_header_state
                        .dispatch(ReverseProxyDisabledHeaderConfigFormAction::SetAll(target_disabled_header));
                }

                let target_cache = rp.cache.as_ref().map_or_else(CacheConfigDto::default, |c| c.clone());
                if cache_state.form != target_cache {
                    cache_state.dispatch(CacheConfigFormAction::SetAll(target_cache));
                }

                let target_rate_limit =
                    rp.rate_limit.as_ref().map_or_else(RateLimitConfigDto::default, |rl| rl.clone());
                if rate_limit_state.form != target_rate_limit {
                    rate_limit_state.dispatch(RateLimitConfigFormAction::SetAll(target_rate_limit));
                }

                let target_resource_retry =
                    rp.resource_retry.as_ref().map_or_else(ResourceRetryConfigDto::default, |rr| rr.clone());
                if resource_retry_state.form != target_resource_retry {
                    resource_retry_state.dispatch(ResourceRetryConfigFormAction::SetAll(target_resource_retry));
                }

                let target_stream = rp.stream.as_ref().map_or_else(StreamConfigDto::default, |s| s.clone());
                if stream_state.form != target_stream {
                    stream_state.dispatch(StreamConfigFormAction::SetAll(target_stream));
                }

                let target_geoip = rp.geoip.as_ref().map_or_else(GeoIpConfigDto::default, |s| s.clone());
                if geoip_state.form != target_geoip {
                    geoip_state.dispatch(GeoIpConfigFormAction::SetAll(target_geoip));
                }

                let target_hls_cache = rp.hls_cache.as_ref().map_or_else(HlsCacheConfigDto::default, |h| h.clone());
                if hls_cache_state.form != target_hls_cache {
                    hls_cache_state.dispatch(HlsCacheConfigFormAction::SetAll(target_hls_cache.clone()));
                }
                if hls_strip_state.form != target_hls_cache.strip {
                    hls_strip_state.dispatch(StripConfigFormAction::SetAll(target_hls_cache.strip));
                }

                let target_stream_buffer = rp.stream.as_ref().and_then(|s| s.buffer.clone()).unwrap_or_default();
                if stream_buffer_state.form != target_stream_buffer {
                    stream_buffer_state.dispatch(StreamBufferConfigFormAction::SetAll(target_stream_buffer));
                }

                let target_failover_patterns = FailoverPatternsDto {
                    patterns: rp
                        .resource_retry
                        .as_ref()
                        .and_then(|rr| rr.failover_redirect_patterns.clone())
                        .unwrap_or_default(),
                };
                if failover_patterns_state.form != target_failover_patterns {
                    failover_patterns_state.dispatch(FailoverPatternsFormAction::SetAll(target_failover_patterns));
                }

                let target_admission_strategies = AdmissionStrategiesDto {
                    strategies: admission_strategy_tags(
                        rp.stream.as_ref().and_then(|stream| stream.admission_strategies.as_ref()),
                    ),
                };
                if admission_strategies_state.form != target_admission_strategies {
                    admission_strategies_state
                        .dispatch(AdmissionStrategiesFormAction::SetAll(target_admission_strategies));
                }

                let target_stream_history =
                    rp.stream_history.as_ref().map_or_else(StreamHistoryConfigDto::default, |s| s.clone());
                if stream_history_state.form != target_stream_history {
                    stream_history_state.dispatch(StreamHistoryConfigFormAction::SetAll(target_stream_history));
                }

                let target_qos_aggregation =
                    rp.qos_aggregation.as_ref().map_or_else(QosAggregationConfigDto::default, |q| q.clone());
                if qos_aggregation_state.form != target_qos_aggregation {
                    qos_aggregation_state.dispatch(QosAggregationConfigFormAction::SetAll(target_qos_aggregation));
                }
            } else {
                let target_reverse_proxy = ReverseProxyConfigDto::default();
                if reverse_proxy_state.form != target_reverse_proxy {
                    reverse_proxy_state.dispatch(ReverseProxyConfigFormAction::SetAll(target_reverse_proxy));
                }

                let target_disabled_header = ReverseProxyDisabledHeaderConfigDto::default();
                if disabled_header_state.form != target_disabled_header {
                    disabled_header_state
                        .dispatch(ReverseProxyDisabledHeaderConfigFormAction::SetAll(target_disabled_header));
                }

                let target_cache = CacheConfigDto::default();
                if cache_state.form != target_cache {
                    cache_state.dispatch(CacheConfigFormAction::SetAll(target_cache));
                }

                let target_rate_limit = RateLimitConfigDto::default();
                if rate_limit_state.form != target_rate_limit {
                    rate_limit_state.dispatch(RateLimitConfigFormAction::SetAll(target_rate_limit));
                }

                let target_resource_retry = ResourceRetryConfigDto::default();
                if resource_retry_state.form != target_resource_retry {
                    resource_retry_state.dispatch(ResourceRetryConfigFormAction::SetAll(target_resource_retry));
                }

                let target_stream = StreamConfigDto::default();
                if stream_state.form != target_stream {
                    stream_state.dispatch(StreamConfigFormAction::SetAll(target_stream));
                }

                let target_geoip = GeoIpConfigDto::default();
                if geoip_state.form != target_geoip {
                    geoip_state.dispatch(GeoIpConfigFormAction::SetAll(target_geoip));
                }

                let target_hls_cache = HlsCacheConfigDto::default();
                if hls_cache_state.form != target_hls_cache {
                    hls_cache_state.dispatch(HlsCacheConfigFormAction::SetAll(target_hls_cache));
                }

                let target_hls_strip = HlsStripConfigDto::default();
                if hls_strip_state.form != target_hls_strip {
                    hls_strip_state.dispatch(StripConfigFormAction::SetAll(target_hls_strip));
                }

                let target_stream_buffer = StreamBufferConfigDto::default();
                if stream_buffer_state.form != target_stream_buffer {
                    stream_buffer_state.dispatch(StreamBufferConfigFormAction::SetAll(target_stream_buffer));
                }

                let target_failover_patterns = FailoverPatternsDto::default();
                if failover_patterns_state.form != target_failover_patterns {
                    failover_patterns_state.dispatch(FailoverPatternsFormAction::SetAll(target_failover_patterns));
                }

                let target_admission_strategies = AdmissionStrategiesDto::default();
                if admission_strategies_state.form != target_admission_strategies {
                    admission_strategies_state
                        .dispatch(AdmissionStrategiesFormAction::SetAll(target_admission_strategies));
                }

                let target_stream_history = StreamHistoryConfigDto::default();
                if stream_history_state.form != target_stream_history {
                    stream_history_state.dispatch(StreamHistoryConfigFormAction::SetAll(target_stream_history));
                }

                let target_qos_aggregation = QosAggregationConfigDto::default();
                if qos_aggregation_state.form != target_qos_aggregation {
                    qos_aggregation_state.dispatch(QosAggregationConfigFormAction::SetAll(target_qos_aggregation));
                }
            }
            || ()
        });
    }

    let render_cache = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RESOURCE_IMAGE_CACHE)}</h1>
                { config_field_bool!(cache_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field_optional!(cache_state.form, translate.t(LABEL_SIZE), size) }
                { config_field_optional!(cache_state.form, translate.t(LABEL_DIRECTORY), directory) }
            </Card>
        }
    };

    let render_hls_cache = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_HLS_CACHE_PROXY)}</h1>
                { config_field_optional!(hls_cache_state.form, translate.t(LABEL_CACHE_PATH), cache_path) }
                { config_field_child!(translate.t(LABEL_STRIP_MODE), "HLS_CACHE_CONFIG.STRIP_MODE", {
                    html! { <span class="tp__form-field__value">{hls_strip_state.form.mode.to_string()}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_STRIP_VALUE), "HLS_CACHE_CONFIG.STRIP_VALUE", {
                    html! { <span class="tp__form-field__value">{hls_strip_state.form.value.to_string()}</span> }
                }) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_CACHE_DURATION), cache_duration) }
                { config_field_child!(translate.t(LABEL_CACHE_BYTES), "HLS_CACHE_CONFIG.CACHE_BYTES", {
                    html! { <span class="tp__form-field__value">{hls_cache_state.form.cache_bytes.as_str().to_string()}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_CACHE_BYTES_PER_SESSION), "HLS_CACHE_CONFIG.CACHE_BYTES_PER_SESSION", {
                    html! { <span class="tp__form-field__value">{hls_cache_state.form.cache_bytes_per_session.as_str().to_string()}</span> }
                }) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_MAX_SEGMENTS_PREFETCH), max_segments_prefetch) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_PER_SESSION), max_concurrent_segment_fetches_per_session) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_GLOBAL), max_concurrent_segment_fetches_global) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_ORIGIN_MANIFEST_TIMEOUT_MS), origin_manifest_timeout_ms) }
                { config_field_child!(translate.t(LABEL_MANIFEST_RECOVERY_BURST), "HLS_CACHE_CONFIG.MANIFEST_RECOVERY_BURST", {
                    html! { <span class="tp__form-field__value">{hls_manifest_recovery_burst_label(hls_cache_state.form.manifest_recovery_burst.level)}</span> }
                }) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_ORIGIN_SEGMENT_TIMEOUT_MS), origin_segment_timeout_ms) }
                { config_field!(hls_cache_state.form, translate.t(LABEL_SESSION_IDLE_TIMEOUT), session_idle_timeout) }
            </Card>
        }
    };

    let render_hls_segment_repair = || {
        let segment_repair = &hls_cache_state.form.segment_repair;
        let watchdog = &segment_repair.corrupt_segment_watchdog;
        let size_increase = hls_segment_repair_size_increase_percent(segment_repair)
            .map_or_else(|| "-".to_string(), |value| format!("{value}%"));
        html! {
            <Card class="tp__config-view__card tp__hls-cache-segment-repair">
                <h1>{translate.t(LABEL_HLS_CACHE_SEGMENT_REPAIR)}</h1>
                { config_field_child!(translate.t(LABEL_SEGMENT_REPAIR), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_MAX_LEVEL", {
                    html! { <span class="tp__form-field__value">{hls_segment_repair_max_level_label(segment_repair.max_level)}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_APPLY_TO_FIRST_SEGMENTS), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_APPLY_TO_FIRST_SEGMENTS", {
                    html! { <span class="tp__form-field__value">{segment_repair.apply_to_first_segments.to_string()}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_MAX_PARALLEL_REPAIRS), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_MAX_PARALLEL_REPAIRS", {
                    html! { <span class="tp__form-field__value">{segment_repair.max_parallel_repairs.to_string()}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_POSTPROCESS_TIMEOUT_MS), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_POSTPROCESS_TIMEOUT_MS", {
                    html! { <span class="tp__form-field__value">{segment_repair.postprocess_timeout_ms.to_string()}</span> }
                }) }
                { config_field_child!(hls_segment_repair_size_increase_label(&translate, segment_repair.max_level), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_SIZE_INCREASE", {
                    html! { <span class="tp__form-field__value">{size_increase}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_REPAIR_TRIGGER), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_TRIGGER", {
                    html! { <span class="tp__form-field__value">{"automatic codec trigger policy"}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_CORRUPT_SEGMENT_WATCHDOG), "HLS_CACHE_CONFIG.CORRUPT_SEGMENT_WATCHDOG", {
                    html! { <span class="tp__form-field__value">{hls_corrupt_segment_watchdog_label(watchdog.mode)}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_MAX_WATCHDOG_JOBS), "HLS_CACHE_CONFIG.CORRUPT_SEGMENT_WATCHDOG_MAX_PARALLEL_JOBS", {
                    html! { <span class="tp__form-field__value">{watchdog.max_parallel_jobs.to_string()}</span> }
                }) }
            </Card>
        }
    };

    let render_hls_cache_edit = || {
        let selected_strip_mode = hls_strip_state.form.mode;
        let strip_state = hls_strip_state.clone();
        let set_max_segments_prefetch = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |value: Option<i64>| {
                let max_segments_prefetch = value.and_then(|value| usize::try_from(value).ok()).unwrap_or(0);
                let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                segment_repair.max_parallel_repairs = segment_repair.max_parallel_repairs.min(max_segments_prefetch);
                segment_repair.corrupt_segment_watchdog.max_parallel_jobs =
                    segment_repair.corrupt_segment_watchdog.max_parallel_jobs.min(max_segments_prefetch.max(1));
                hls_cache_state.dispatch(HlsCacheConfigFormAction::MaxSegmentsPrefetch(max_segments_prefetch));
                hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
            })
        };
        let edit_hls_cache_u64_min =
            |label: String, field: &'static str, value: u64, action: fn(u64) -> HlsCacheConfigFormAction| {
                let hls_cache_state = hls_cache_state.clone();
                html! {
                    <div class="tp__form-field tp__form-field__number">
                        <NumberInput
                            label={label}
                            name={field}
                            field_id={Some(dto_field_id(&hls_cache_state.form, field))}
                            value={value.min(i64::MAX as u64) as i64}
                            on_change={Callback::from(move |value: Option<i64>| {
                                hls_cache_state.dispatch(action(clamp_u64_min(value, 1)));
                            })}
                        />
                    </div>
                }
            };
        let edit_hls_cache_usize_min =
            |label: String, field: &'static str, value: usize, action: fn(usize) -> HlsCacheConfigFormAction| {
                let hls_cache_state = hls_cache_state.clone();
                html! {
                    <div class="tp__form-field tp__form-field__number">
                        <NumberInput
                            label={label}
                            name={field}
                            field_id={Some(dto_field_id(&hls_cache_state.form, field))}
                            value={value.min(i64::MAX as usize) as i64}
                            on_change={Callback::from(move |value: Option<i64>| {
                                hls_cache_state.dispatch(action(clamp_usize_min(value, 1)));
                            })}
                        />
                    </div>
                }
            };
        let selected_manifest_recovery_burst = hls_cache_state.form.manifest_recovery_burst.level;
        let set_manifest_recovery_burst = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |(_, selections): (String, DropDownSelection)| {
                if let DropDownSelection::Single(selection) = selections {
                    if let Ok(level) = HlsManifestRecoveryBurstLevel::from_str(selection.as_str()) {
                        hls_cache_state.dispatch(HlsCacheConfigFormAction::ManifestRecoveryBurst(
                            HlsManifestRecoveryBurstConfigDto { level },
                        ));
                    }
                }
            })
        };
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_HLS_CACHE_PROXY)}</h1>
                { edit_field_text_option!(hls_cache_state, translate.t(LABEL_CACHE_PATH), cache_path, HlsCacheConfigFormAction::CachePath) }
                { config_field_child!(translate.t(LABEL_STRIP_MODE), "HLS_CACHE_CONFIG.STRIP_MODE", {
                    html! {
                        <Select
                            name="hls_strip_mode"
                            multi_select={false}
                            options={hls_strip_mode_options(selected_strip_mode)}
                            on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                                if let DropDownSelection::Single(selection) = selections {
                                    if let Ok(mode) = HlsStripMode::from_str(selection.as_str()) {
                                        strip_state.dispatch(StripConfigFormAction::Mode(mode));
                                    }
                                }
                            })}
                        />
                    }
                }) }
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_STRIP_VALUE)}
                        name="hls_strip_value"
                        field_id={Some("HLS_CACHE_CONFIG.STRIP_VALUE".to_string())}
                        value={hls_strip_state.form.value.min(i64::MAX as u64) as i64}
                        on_change={Callback::from({
                            let hls_strip_state = hls_strip_state.clone();
                            move |value: Option<i64>| {
                                hls_strip_state.dispatch(StripConfigFormAction::Value(clamp_u64_min(value, 0)));
                            }
                        })}
                    />
                </div>
                { edit_hls_cache_u64_min(translate.t(LABEL_CACHE_DURATION), "cache_duration", hls_cache_state.form.cache_duration, HlsCacheConfigFormAction::CacheDuration) }
                { edit_field_text!(hls_cache_state, translate.t(LABEL_CACHE_BYTES), cache_bytes, HlsCacheConfigFormAction::CacheBytes) }
                { edit_field_text!(hls_cache_state, translate.t(LABEL_CACHE_BYTES_PER_SESSION), cache_bytes_per_session, HlsCacheConfigFormAction::CacheBytesPerSession) }
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_MAX_SEGMENTS_PREFETCH)}
                        name="max_segments_prefetch"
                        field_id={Some(dto_field_id(&hls_cache_state.form, "max_segments_prefetch"))}
                        value={hls_cache_state.form.max_segments_prefetch.min(i64::MAX as usize) as i64}
                        on_change={set_max_segments_prefetch}
                    />
                </div>
                { edit_hls_cache_usize_min(translate.t(LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_PER_SESSION), "max_concurrent_segment_fetches_per_session", hls_cache_state.form.max_concurrent_segment_fetches_per_session, HlsCacheConfigFormAction::MaxConcurrentSegmentFetchesPerSession) }
                { edit_hls_cache_usize_min(translate.t(LABEL_MAX_CONCURRENT_SEGMENT_FETCHES_GLOBAL), "max_concurrent_segment_fetches_global", hls_cache_state.form.max_concurrent_segment_fetches_global, HlsCacheConfigFormAction::MaxConcurrentSegmentFetchesGlobal) }
                { edit_hls_cache_u64_min(translate.t(LABEL_ORIGIN_MANIFEST_TIMEOUT_MS), "origin_manifest_timeout_ms", hls_cache_state.form.origin_manifest_timeout_ms, HlsCacheConfigFormAction::OriginManifestTimeoutMs) }
                { config_field_child!(translate.t(LABEL_MANIFEST_RECOVERY_BURST), "HLS_CACHE_CONFIG.MANIFEST_RECOVERY_BURST", {
                    html! {
                        <Select
                            name="hls_manifest_recovery_burst"
                            multi_select={false}
                            options={hls_manifest_recovery_burst_options(selected_manifest_recovery_burst)}
                            on_select={set_manifest_recovery_burst}
                        />
                    }
                }) }
                { edit_hls_cache_u64_min(translate.t(LABEL_ORIGIN_SEGMENT_TIMEOUT_MS), "origin_segment_timeout_ms", hls_cache_state.form.origin_segment_timeout_ms, HlsCacheConfigFormAction::OriginSegmentTimeoutMs) }
                { edit_hls_cache_u64_min(translate.t(LABEL_SESSION_IDLE_TIMEOUT), "session_idle_timeout", hls_cache_state.form.session_idle_timeout, HlsCacheConfigFormAction::SessionIdleTimeout) }
            </Card>
        }
    };

    let render_hls_segment_repair_edit = || {
        let segment_repair = hls_cache_state.form.segment_repair.clone();
        let watchdog = segment_repair.corrupt_segment_watchdog.clone();
        let selected_segment_repair_max_level = segment_repair.max_level;
        let set_segment_repair_max_level = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |(_, selections): (String, DropDownSelection)| {
                if let DropDownSelection::Single(selection) = selections {
                    if let Ok(mode) = HlsSegmentRepairMode::from_str(selection.as_str()) {
                        let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                        segment_repair.max_level = mode;
                        hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
                    }
                }
            })
        };
        let set_segment_repair_apply_to_first_segments = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |value: Option<i64>| {
                let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                segment_repair.apply_to_first_segments = clamp_u8_range(value, 0, 6);
                hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
            })
        };
        let set_segment_repair_max_parallel_repairs = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |value: Option<i64>| {
                let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                segment_repair.max_parallel_repairs =
                    clamp_usize_min(value, 1).min(hls_cache_state.form.max_segments_prefetch);
                hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
            })
        };
        let set_segment_repair_postprocess_timeout_ms = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |value: Option<i64>| {
                let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                segment_repair.postprocess_timeout_ms = clamp_u64_min(value, 100);
                hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
            })
        };
        let selected_watchdog_mode = watchdog.mode;
        let set_watchdog_mode = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |(_, selections): (String, DropDownSelection)| {
                if let DropDownSelection::Single(selection) = selections {
                    if let Ok(mode) = HlsCorruptSegmentWatchdogMode::from_str(selection.as_str()) {
                        let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                        segment_repair.corrupt_segment_watchdog.mode = mode;
                        hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
                    }
                }
            })
        };
        let set_watchdog_max_parallel_jobs = {
            let hls_cache_state = hls_cache_state.clone();
            Callback::from(move |value: Option<i64>| {
                let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                segment_repair.corrupt_segment_watchdog.max_parallel_jobs =
                    clamp_usize_min(value, 1).min(hls_cache_state.form.max_segments_prefetch.max(1));
                hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
            })
        };
        let render_slider_control = |name: String, value: u8, max_value: u8, on_change: Callback<u8>| -> Html {
            let value_string = value.to_string();
            let max_string = max_value.to_string();
            let fill = if max_value == 0 { 0 } else { (u16::from(value) * 100) / u16::from(max_value) };
            let slider_style = format!("--tp-hls-repair-slider-fill: {fill}%;");
            let oninput = Callback::from(move |event: InputEvent| {
                let input: HtmlInputElement = event.target_unchecked_into();
                if let Ok(value) = input.value().parse::<u8>() {
                    on_change.emit(value.min(max_value));
                }
            });
            html! {
                <div class="tp__hls-repair-slider">
                    <input
                        class="tp__hls-repair-slider__range"
                        type="range"
                        name={name}
                        min="0"
                        max={max_string}
                        value={value_string.clone()}
                        style={slider_style}
                        oninput={oninput}
                    />
                    <span class="tp__form-field__value tp__hls-repair-slider__value">{value_string}</span>
                </div>
            }
        };
        let render_slider =
            |label: String, info_key: &'static str, name: String, value: u8, max_value: u8, on_change: Callback<u8>| {
                let control = render_slider_control(name, value, max_value, on_change);
                config_field_child!(label, info_key, {
                    html! {
                        { control }
                    }
                })
            };
        let size_increase_editor =
            if let Some(size_increase_percent) = hls_segment_repair_size_increase_percent(&segment_repair) {
                let hls_cache_state = hls_cache_state.clone();
                render_slider(
                    hls_segment_repair_size_increase_label(&translate, segment_repair.max_level),
                    "HLS_CACHE_CONFIG.SEGMENT_REPAIR_SIZE_INCREASE",
                    "hls_segment_repair_size_increase".to_string(),
                    size_increase_percent,
                    100,
                    Callback::from(move |value| {
                        let mut segment_repair = hls_cache_state.form.segment_repair.clone();
                        set_hls_segment_repair_size_increase_percent(&mut segment_repair, value);
                        hls_cache_state.dispatch(HlsCacheConfigFormAction::SegmentRepair(segment_repair));
                    }),
                )
            } else {
                config_field_child!(
                    translate.t(LABEL_SEGMENT_SIZE_INCREASE),
                    "HLS_CACHE_CONFIG.SEGMENT_REPAIR_SIZE_INCREASE",
                    {
                        html! { <span class="tp__form-field__value">{"-"}</span> }
                    }
                )
            };

        html! {
            <Card class="tp__config-view__card tp__hls-cache-segment-repair">
                <h1>{translate.t(LABEL_HLS_CACHE_SEGMENT_REPAIR)}</h1>
                { config_field_child!(translate.t(LABEL_SEGMENT_REPAIR), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_MAX_LEVEL", {
                    html! {
                        <Select
                            name="hls_segment_repair_max_level"
                            multi_select={false}
                            options={hls_segment_repair_max_level_options(selected_segment_repair_max_level)}
                            on_select={set_segment_repair_max_level}
                        />
                    }
                }) }
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_APPLY_TO_FIRST_SEGMENTS)}
                        name="hls_segment_repair_apply_to_first_segments"
                        field_id={Some("HLS_CACHE_CONFIG.SEGMENT_REPAIR_APPLY_TO_FIRST_SEGMENTS".to_string())}
                        value={i64::from(segment_repair.apply_to_first_segments)}
                        on_change={set_segment_repair_apply_to_first_segments}
                    />
                </div>
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_MAX_PARALLEL_REPAIRS)}
                        name="hls_segment_repair_max_parallel_repairs"
                        field_id={Some("HLS_CACHE_CONFIG.SEGMENT_REPAIR_MAX_PARALLEL_REPAIRS".to_string())}
                        value={segment_repair.max_parallel_repairs.min(i64::MAX as usize) as i64}
                        on_change={set_segment_repair_max_parallel_repairs}
                    />
                </div>
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_POSTPROCESS_TIMEOUT_MS)}
                        name="hls_segment_repair_postprocess_timeout_ms"
                        field_id={Some("HLS_CACHE_CONFIG.SEGMENT_REPAIR_POSTPROCESS_TIMEOUT_MS".to_string())}
                        value={segment_repair.postprocess_timeout_ms.min(i64::MAX as u64) as i64}
                        on_change={set_segment_repair_postprocess_timeout_ms}
                    />
                </div>
                { size_increase_editor }
                { config_field_child!(translate.t(LABEL_REPAIR_TRIGGER), "HLS_CACHE_CONFIG.SEGMENT_REPAIR_TRIGGER", {
                    html! { <span class="tp__form-field__value">{"automatic codec trigger policy"}</span> }
                }) }
                { config_field_child!(translate.t(LABEL_CORRUPT_SEGMENT_WATCHDOG), "HLS_CACHE_CONFIG.CORRUPT_SEGMENT_WATCHDOG", {
                    html! {
                        <Select
                            name="hls_corrupt_segment_watchdog"
                            multi_select={false}
                            options={hls_corrupt_segment_watchdog_options(selected_watchdog_mode)}
                            on_select={set_watchdog_mode}
                        />
                    }
                }) }
                <div class="tp__form-field tp__form-field__number">
                    <NumberInput
                        label={translate.t(LABEL_MAX_WATCHDOG_JOBS)}
                        name="hls_corrupt_segment_watchdog_max_parallel_jobs"
                        field_id={Some("HLS_CACHE_CONFIG.CORRUPT_SEGMENT_WATCHDOG_MAX_PARALLEL_JOBS".to_string())}
                        value={watchdog.max_parallel_jobs.min(i64::MAX as usize) as i64}
                        on_change={set_watchdog_max_parallel_jobs}
                    />
                </div>
            </Card>
        }
    };

    let render_stream = || {
        let strategy_tags = displayed_admission_strategy_tags(&admission_strategies_state.form, &stream_state.form);
        html! {
            <>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM)}</h1>
                { config_field_bool!(stream_state.form, translate.t(LABEL_STREAM_METRICS_ENABLED), metrics_enabled) }
                { config_field_bool!(stream_state.form, translate.t(LABEL_RETRY), retry) }
                { config_field_optional!(stream_state.form, translate.t(LABEL_THROTTLE), throttle) }
                { config_field!(stream_state.form, translate.t(LABEL_THROTTLE_KBPS), throttle_kbps) }
                { config_field!(stream_state.form, translate.t(LABEL_SHARED_BURST_BUFFER_MB), shared_burst_buffer_mb) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_GRACE)}</h1>
                { config_field!(stream_state.form, translate.t(LABEL_GRACE_PERIOD_MILLIS), grace_period_millis) }
                { config_field!(stream_state.form, translate.t(LABEL_GRACE_PERIOD_TIMEOUT_SECS), grace_period_timeout_secs) }
                { config_field_bool!(stream_state.form, translate.t(LABEL_GRACE_PERIOD_HOLD_STREAM), grace_period_hold_stream) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_SESSION)}</h1>
                { config_field!(stream_state.form, translate.t(LABEL_HLS_SESSION_TTL_SECS), hls_session_ttl_secs) }
                { config_field!(stream_state.form, translate.t(LABEL_CATCHUP_SESSION_TTL_SECS), catchup_session_ttl_secs) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_ADMISSION_STRATEGIES)}</h1>
                { config_field_child!(translate.t(LABEL_ADMISSION_STRATEGIES), "REVERSE_PROXY_CONFIG.ADMISSION_STRATEGIES", {
                    html! {
                        <div class="tp__config-view__tags">
                        if strategy_tags.is_empty() {
                            <Chip label="-" />
                        } else {
                            { for strategy_tags.iter().map(|strategy| {
                                html! { <Chip label={admission_strategy_tag_label(&translate, strategy)} /> }
                            }) }
                        }
                        </div>
                    }
                })}
            </Card>
            </>
        }
    };
    let render_stream_buffer = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_BUFFER)}</h1>
                { config_field_bool!(stream_buffer_state.form, translate.t(LABEL_BUFFER_ENABLED), enabled) }
                { config_field!(stream_buffer_state.form, translate.t(LABEL_BUFFER_SIZE), size) }
            </Card>
        }
    };

    let render_rate_limit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RATE_LIMIT)}</h1>
                { config_field_bool!(rate_limit_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field!(rate_limit_state.form, translate.t(LABEL_PERIOD_MILLIS), period_millis) }
                { config_field!(rate_limit_state.form, translate.t(LABEL_BURST_SIZE), burst_size) }
            </Card>
        }
    };

    let render_geoip = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_GEOIP)}</h1>
                { config_field_bool!(geoip_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field!(geoip_state.form, translate.t(LABEL_URL), url) }
                { config_field_child!(translate.t(LABEL_GEOIP_UNAVAILABLE_POLICY), "GEO_IP_CONFIG.UNAVAILABLE_POLICY", {
                    html! {
                        <span class="tp__form-field__value">
                            {geoip_unavailable_policy_label(&translate, geoip_state.form.unavailable_policy)}
                        </span>
                    }
                }) }
            </Card>
        }
    };

    let render_settings_view = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_SETTINGS)}</h1>
                { config_field_bool!(reverse_proxy_state.form, translate.t(LABEL_RESOURCE_REWRITE_DISABLED), resource_rewrite_disabled) }
                { config_field_hide!(reverse_proxy_state.form, translate.t(LABEL_REWRITE_SECRET), rewrite_secret) }
            </Card>
        }
    };

    let render_settings_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_SETTINGS)}</h1>
                { edit_field_bool!(reverse_proxy_state, translate.t(LABEL_RESOURCE_REWRITE_DISABLED), resource_rewrite_disabled, ReverseProxyConfigFormAction::ResourceRewriteDisabled) }
                { edit_field_text!(reverse_proxy_state, translate.t(LABEL_REWRITE_SECRET), rewrite_secret, ReverseProxyConfigFormAction::RewriteSecret, true) }
            </Card>
        }
    };

    let render_resource_retry_view = || {
        let patterns = &failover_patterns_state.form.patterns;
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RESOURCE_RETRY)}</h1>
                { config_field!(resource_retry_state.form, translate.t(LABEL_MAX_ATTEMPTS), max_attempts) }
                { config_field!(resource_retry_state.form, translate.t(LABEL_BACKOFF_MILLIS), backoff_millis) }
                {
                    config_field_custom!(
                        translate.t(LABEL_BACKOFF_MULTIPLIER),
                        format_float_localized(resource_retry_state.form.backoff_multiplier, 4, true)
                    )
                }
                { config_field_child!(translate.t(LABEL_FAILOVER_REDIRECT_PATTERNS), "REVERSE_PROXY_CONFIG.FAILOVER_REDIRECT_PATTERNS", {
                    html! {
                        <div class="tp__config-view__tags">
                        if patterns.is_empty() {
                            <Chip label="service-abuse (default)" />
                        } else {
                            { for patterns.iter().map(|p| html! { <Chip label={p.clone()} /> }) }
                        }
                        </div>
                    }
                })}
            </Card>
        }
    };

    let render_resource_retry_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RESOURCE_RETRY)}</h1>
                { edit_field_number!(resource_retry_state, translate.t(LABEL_MAX_ATTEMPTS), max_attempts, ResourceRetryConfigFormAction::MaxAttempts) }
                { edit_field_number_u64!(resource_retry_state, translate.t(LABEL_BACKOFF_MILLIS), backoff_millis, ResourceRetryConfigFormAction::BackoffMillis) }
                { edit_field_number_f64!(resource_retry_state, translate.t(LABEL_BACKOFF_MULTIPLIER), backoff_multiplier, ResourceRetryConfigFormAction::BackoffMultiplier) }
                { edit_field_list!(failover_patterns_state, translate.t(LABEL_FAILOVER_REDIRECT_PATTERNS), patterns, FailoverPatternsFormAction::Patterns, translate.t(LABEL_ADD_PATTERN)) }
            </Card>
        }
    };

    let render_disabled_header_view = || {
        let custom_headers = if disabled_header_state.form.custom_header.is_empty() {
            "-".to_string()
        } else {
            disabled_header_state.form.custom_header.join(", ")
        };
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DISABLED_HEADER)}</h1>
                { config_field_bool!(disabled_header_state.form, translate.t(LABEL_REFERER_HEADER), referer_header) }
                { config_field_bool!(disabled_header_state.form, translate.t(LABEL_X_HEADER), x_header) }
                { config_field_bool!(disabled_header_state.form, translate.t(LABEL_CF_HEADER), cloudflare_header) }
                { config_field_custom!(translate.t(LABEL_CUSTOM_HEADERS), custom_headers) }
            </Card>
        }
    };

    let render_disabled_header_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DISABLED_HEADER)}</h1>
                { edit_field_bool!(disabled_header_state, translate.t(LABEL_REFERER_HEADER), referer_header, ReverseProxyDisabledHeaderConfigFormAction::RefererHeader) }
                { edit_field_bool!(disabled_header_state, translate.t(LABEL_X_HEADER), x_header, ReverseProxyDisabledHeaderConfigFormAction::XHeader) }
                { edit_field_bool!(disabled_header_state, translate.t(LABEL_CF_HEADER), cloudflare_header, ReverseProxyDisabledHeaderConfigFormAction::CloudflareHeader) }
                { edit_field_list!(disabled_header_state, translate.t(LABEL_CUSTOM_HEADERS), custom_header, ReverseProxyDisabledHeaderConfigFormAction::CustomHeader, translate.t(LABEL_ADD_HEADER)) }
            </Card>
        }
    };

    let render_geoip_edit = || {
        let geoip_policy_state = geoip_state.clone();
        let selected_policy = Rc::new(vec![geoip_state.form.unavailable_policy.to_string()]);
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_GEOIP)}</h1>
                { edit_field_bool!(geoip_state, translate.t(LABEL_ENABLED), enabled, GeoIpConfigFormAction::Enabled) }
                { edit_field_text!(geoip_state, translate.t(LABEL_URL), url, GeoIpConfigFormAction::Url) }
                { config_field_child!(translate.t(LABEL_GEOIP_UNAVAILABLE_POLICY), "GEO_IP_CONFIG.UNAVAILABLE_POLICY", {
                    html! {
                        <RadioButtonGroup
                            multi_select={false}
                            none_allowed={false}
                            options={geoip_unavailable_policy_options()}
                            labels={Some(geoip_unavailable_policy_labels(&translate))}
                            selected={selected_policy}
                            on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                                if let Some(selection) = selections.first() {
                                    geoip_policy_state.dispatch(GeoIpConfigFormAction::UnavailablePolicy(
                                        GeoIpUnavailablePolicy::from_str(selection).unwrap_or(GeoIpUnavailablePolicy::Deny),
                                    ));
                                }
                            })}
                        />
                    }
                }) }
            </Card>
        }
    };

    let render_cache_edit = || {
        html! {
          <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_CACHE)}</h1>
            { edit_field_bool!(cache_state, translate.t(LABEL_ENABLED), enabled, CacheConfigFormAction::Enabled) }
            { edit_field_byte_size_option!(cache_state, translate.t(LABEL_SIZE), size, CacheConfigFormAction::Size) }
            { edit_field_text_option!(cache_state, translate.t(LABEL_DIRECTORY), directory, CacheConfigFormAction::Dir) }
          </Card>
        }
    };

    let render_rate_limit_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_RATE_LIMIT)}</h1>
                { edit_field_bool!(rate_limit_state, translate.t(LABEL_ENABLED), enabled, RateLimitConfigFormAction::Enabled) }
                { edit_field_number_u64!(rate_limit_state, translate.t(LABEL_PERIOD_MILLIS), period_millis, RateLimitConfigFormAction::PeriodMillis) }
                { edit_field_number!(rate_limit_state, translate.t(LABEL_BURST_SIZE), burst_size, RateLimitConfigFormAction::BurstSize) }
            </Card>
        }
    };

    let render_stream_edit = || {
        let strategy_tags = displayed_admission_strategy_tags(&admission_strategies_state.form, &stream_state.form);
        let available_strategies =
            available_admission_strategies(&strategy_tags, stream_state.form.grace_period_millis);
        html! {
            <>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM)}</h1>
                { edit_field_bool!(stream_state, translate.t(LABEL_STREAM_METRICS_ENABLED), metrics_enabled, StreamConfigFormAction::MetricsEnabled) }
                { edit_field_bool!(stream_state, translate.t(LABEL_RETRY), retry, StreamConfigFormAction::Retry) }
                { edit_field_text_option!(stream_state, translate.t(LABEL_THROTTLE), throttle, StreamConfigFormAction::Throttle) }
                { edit_field_number_u64!(stream_state, translate.t(LABEL_THROTTLE_KBPS), throttle_kbps, StreamConfigFormAction::ThrottleKbps) }
                { edit_field_number_u64!(stream_state, translate.t(LABEL_SHARED_BURST_BUFFER_MB), shared_burst_buffer_mb, StreamConfigFormAction::SharedBurstBufferMb) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_GRACE)}</h1>
                { edit_field_number_u64!(stream_state, translate.t(LABEL_GRACE_PERIOD_MILLIS), grace_period_millis, StreamConfigFormAction::GracePeriodMillis) }
                { edit_field_number_u64!(stream_state, translate.t(LABEL_GRACE_PERIOD_TIMEOUT_SECS), grace_period_timeout_secs, StreamConfigFormAction::GracePeriodTimeoutSecs) }
                { edit_field_bool!(stream_state, translate.t(LABEL_GRACE_PERIOD_HOLD_STREAM), grace_period_hold_stream, StreamConfigFormAction::GracePeriodHoldStream) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_SESSION)}</h1>
                { edit_field_number_u64!(stream_state, translate.t(LABEL_HLS_SESSION_TTL_SECS), hls_session_ttl_secs, StreamConfigFormAction::HlsSessionTtlSecs) }
                { edit_field_number_u64!(stream_state, translate.t(LABEL_CATCHUP_SESSION_TTL_SECS), catchup_session_ttl_secs, StreamConfigFormAction::CatchupSessionTtlSecs) }
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_ADMISSION_STRATEGIES)}</h1>
                <div class="tp__config-view__tags">
                if strategy_tags.is_empty() {
                    <Chip label="-" />
                } else {
                    { for strategy_tags.iter().enumerate().map(|(index, strategy)| {
                        let remove_state = admission_strategies_state.clone();
                        let remove_tags = strategy_tags.clone();
                        let move_up_state = admission_strategies_state.clone();
                        let move_up_tags = strategy_tags.clone();
                        let move_down_state = admission_strategies_state.clone();
                        let move_down_tags = strategy_tags.clone();
                        let label = admission_strategy_tag_label(&translate, strategy);
                        html! {
                            <div class="tp__inline-toolbar">
                                <Chip label={label} />
                                if index > 0 {
                                    <IconButton
                                        name={format!("move_up_{index}")}
                                        icon="ChevronUp"
                                        hint={translate.t("LABEL.MOVE_UP")}
                                        onclick={Callback::from(move |_| {
                                            move_up_state.dispatch(AdmissionStrategiesFormAction::Strategies(Some(
                                                move_admission_strategy_tag(&move_up_tags, index, -1)
                                            )));
                                        })}
                                    />
                                }
                                if index + 1 < strategy_tags.len() {
                                    <IconButton
                                        name={format!("move_down_{index}")}
                                        icon="ChevronDown"
                                        hint={translate.t("LABEL.MOVE_DOWN")}
                                        onclick={Callback::from(move |_| {
                                            move_down_state.dispatch(AdmissionStrategiesFormAction::Strategies(Some(
                                                move_admission_strategy_tag(&move_down_tags, index, 1)
                                            )));
                                        })}
                                    />
                                }
                                <IconButton
                                    class="secondary"
                                    name={format!("remove_{index}")}
                                    icon="Delete"
                                    hint="Remove"
                                    onclick={Callback::from(move |_| {
                                        remove_state.dispatch(AdmissionStrategiesFormAction::Strategies(Some(
                                            remove_admission_strategy_tag(&remove_tags, index)
                                        )));
                                    })}
                                />
                            </div>
                        }
                    }) }
                }
                </div>
                <div class="tp__toolbar">
                {
                    for available_strategies.into_iter().map(|strategy| {
                        let add_state = admission_strategies_state.clone();
                        let add_tags = strategy_tags.clone();
                        let strategy_name = admission_strategy_label(&translate, strategy);
                        html! {
                            <TextButton
                                class="primary"
                                name={strategy_name.clone()}
                                icon="Add"
                                title={strategy_name}
                                onclick={Callback::from(move |_| {
                                    add_state.dispatch(AdmissionStrategiesFormAction::Strategies(Some(
                                        add_admission_strategy_tag(&add_tags, strategy)
                                    )));
                                })}
                            />
                        }
                    })
                }
                </div>
            </Card>
            </>
        }
    };
    let render_stream_buffer_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_BUFFER)}</h1>
                { edit_field_bool!(stream_buffer_state, translate.t(LABEL_BUFFER_ENABLED), enabled, StreamBufferConfigFormAction::Enabled) }
                { edit_field_number_usize!(stream_buffer_state, translate.t(LABEL_BUFFER_SIZE), size, StreamBufferConfigFormAction::Size) }
            </Card>
        }
    };

    let render_stream_history = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_HISTORY)}</h1>
                { config_field_bool!(stream_history_state.form, translate.t(LABEL_STREAM_HISTORY_ENABLED), stream_history_enabled) }
                { config_field!(stream_history_state.form, translate.t(LABEL_STREAM_HISTORY_BATCH_SIZE), stream_history_batch_size) }
                { config_field!(stream_history_state.form, translate.t(LABEL_STREAM_HISTORY_RETENTION_DAYS), stream_history_retention_days) }
                { config_field!(stream_history_state.form, translate.t(LABEL_DIRECTORY), stream_history_directory) }
            </Card>
        }
    };

    let render_qos_aggregation = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_QOS_AGGREGATION)}</h1>
                { config_field_bool!(qos_aggregation_state.form, translate.t(LABEL_QOS_AGGREGATION_ENABLED), enabled) }
                { config_field!(qos_aggregation_state.form, translate.t(LABEL_INTERVAL_SECS), interval_secs) }
            </Card>
        }
    };

    let render_stream_history_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM_HISTORY)}</h1>
                { edit_field_bool!(stream_history_state, translate.t(LABEL_STREAM_HISTORY_ENABLED), stream_history_enabled, StreamHistoryConfigFormAction::Enabled) }
                { edit_field_number_usize!(stream_history_state, translate.t(LABEL_STREAM_HISTORY_BATCH_SIZE), stream_history_batch_size, StreamHistoryConfigFormAction::BatchSize) }
                { edit_field_number_u16!(stream_history_state, translate.t(LABEL_STREAM_HISTORY_RETENTION_DAYS), stream_history_retention_days, StreamHistoryConfigFormAction::RetentionDays) }
                { edit_field_text!(stream_history_state, translate.t(LABEL_DIRECTORY), stream_history_directory, StreamHistoryConfigFormAction::Directory) }
            </Card>
        }
    };

    let render_qos_aggregation_edit = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_QOS_AGGREGATION)}</h1>
                { edit_field_bool!(qos_aggregation_state, translate.t(LABEL_QOS_AGGREGATION_ENABLED), enabled, QosAggregationConfigFormAction::Enabled) }
                { edit_field_number_u64!(qos_aggregation_state, translate.t(LABEL_INTERVAL_SECS), interval_secs, QosAggregationConfigFormAction::IntervalSecs) }
            </Card>
        }
    };

    let render_view_mode = || {
        html! {
            <div class="tp__reverse-proxy-config-view__body tp__config-view-page__body">
                { render_settings_view() }
                { render_geoip() }
                { render_disabled_header_view() }
                { render_cache() }
                { render_hls_cache() }
                { render_hls_segment_repair() }
                { render_resource_retry_view() }
                { render_rate_limit() }
                { render_stream() }
                { render_stream_buffer() }
                { render_stream_history() }
                { render_qos_aggregation() }
            </div>
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__reverse-proxy-config-view__body tp__config-view-page__body">
                { render_settings_edit() }
                { render_geoip_edit() }
                { render_disabled_header_edit() }
                { render_cache_edit() }
                { render_hls_cache_edit() }
                { render_hls_segment_repair_edit() }
                { render_resource_retry_edit() }
                { render_rate_limit_edit() }
                { render_stream_edit() }
                { render_stream_buffer_edit() }
                { render_stream_history_edit() }
                { render_qos_aggregation_edit() }
            </div>
        }
    };

    html! {
        <div class="tp__reverse-proxy-config-view tp__config-view-page">
        <div class="tp__config-view-page__title">{translate.t(LABEL_REVERSE_PROXY_CONFIG)}</div>
            {
                if *config_view_ctx.edit_mode {
                    render_edit_mode()
                } else {
                    render_view_mode()
                }
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geoip_unavailable_policy_roundtrips_through_string_representation() {
        for policy in GeoIpUnavailablePolicy::iter() {
            let parsed =
                GeoIpUnavailablePolicy::from_str(policy.as_ref()).expect("failed to parse GeoIpUnavailablePolicy");
            assert_eq!(parsed, policy);
        }
    }
}
