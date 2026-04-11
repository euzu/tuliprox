#![allow(clippy::large_enum_variant)]

use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_REVERSE_PROXY_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_mapped_option,
            },
            Card, Chip, IconButton, TextButton,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_hide, config_field_optional,
    edit_field_bool, edit_field_list, edit_field_number, edit_field_number_f64, edit_field_number_u16,
    edit_field_number_u64, edit_field_number_usize, edit_field_text, edit_field_text_option, generate_form_reducer,
    i18n::{use_translation, YewI18n},
    utils::t_safe,
};
use shared::{
    model::{
        AdmissionStrategyDto, CacheConfigDto, GeoIpConfigDto, QosAggregationConfigDto, RateLimitConfigDto,
        ResourceRetryConfigDto, ReverseProxyConfigDto, ReverseProxyDisabledHeaderConfigDto, StreamBufferConfigDto,
        StreamConfigDto, StreamHistoryConfigDto,
    },
    utils::{default_secret, format_float_localized},
};
use yew::prelude::*;

const LABEL_CACHE: &str = "LABEL.CACHE";
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
const LABEL_URL: &str = "LABEL.URL";

const LABEL_STREAM_HISTORY: &str = "LABEL.STREAM_HISTORY";
const LABEL_STREAM_HISTORY_ENABLED: &str = "LABEL.STREAM_HISTORY_ENABLED";
const LABEL_STREAM_HISTORY_BATCH_SIZE: &str = "LABEL.STREAM_HISTORY_BATCH_SIZE";
const LABEL_STREAM_HISTORY_RETENTION_DAYS: &str = "LABEL.STREAM_HISTORY_RETENTION_DAYS";
const LABEL_QOS_AGGREGATION: &str = "LABEL.QOS_AGGREGATION";
const LABEL_QOS_AGGREGATION_ENABLED: &str = "LABEL.QOS_AGGREGATION_ENABLED";
const LABEL_INTERVAL_SECS: &str = "LABEL.INTERVAL_SECS";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_OLDEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_OLDEST";
const LABEL_ADMISSION_STRATEGY_EVICT_USER_LATEST: &str = "LABEL.ADMISSION_STRATEGY_EVICT_USER_LATEST";
const LABEL_ADMISSION_STRATEGY_GRACE_INSTANT_STREAM: &str = "LABEL.ADMISSION_STRATEGY_GRACE_INSTANT_STREAM";
const LABEL_ADMISSION_STRATEGY_GRACE_HOLD_STREAM: &str = "LABEL.ADMISSION_STRATEGY_GRACE_HOLD_STREAM";

generate_form_reducer!(
    state: CacheConfigFormState { form: CacheConfigDto },
    action_name: CacheConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Size => size: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdmissionStrategiesDto {
    pub strategies: Option<Vec<String>>,
}

fn admission_strategy_tag(strategy: AdmissionStrategyDto) -> &'static str {
    match strategy {
        AdmissionStrategyDto::EvictUserSameIpOldest => "evict_user_same_ip_oldest",
        AdmissionStrategyDto::EvictUserSameIpLatest => "evict_user_same_ip_latest",
        AdmissionStrategyDto::EvictUserOldest => "evict_user_oldest",
        AdmissionStrategyDto::EvictUserLatest => "evict_user_latest",
        AdmissionStrategyDto::GraceInstantStream => "grace_instant_stream",
        AdmissionStrategyDto::GraceHoldStream => "grace_hold_stream",
    }
}

fn parse_admission_strategy_tag(tag: &str) -> Option<AdmissionStrategyDto> {
    match tag.trim() {
        "evict_user_same_ip_oldest" => Some(AdmissionStrategyDto::EvictUserSameIpOldest),
        "evict_user_same_ip_latest" => Some(AdmissionStrategyDto::EvictUserSameIpLatest),
        "evict_user_oldest" => Some(AdmissionStrategyDto::EvictUserOldest),
        "evict_user_latest" => Some(AdmissionStrategyDto::EvictUserLatest),
        "grace_instant_stream" => Some(AdmissionStrategyDto::GraceInstantStream),
        "grace_hold_stream" => Some(AdmissionStrategyDto::GraceHoldStream),
        _ => None,
    }
}

fn admission_strategy_label_key(strategy: AdmissionStrategyDto) -> &'static str {
    match strategy {
        AdmissionStrategyDto::EvictUserSameIpOldest => LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_OLDEST,
        AdmissionStrategyDto::EvictUserSameIpLatest => LABEL_ADMISSION_STRATEGY_EVICT_USER_SAME_IP_LATEST,
        AdmissionStrategyDto::EvictUserOldest => LABEL_ADMISSION_STRATEGY_EVICT_USER_OLDEST,
        AdmissionStrategyDto::EvictUserLatest => LABEL_ADMISSION_STRATEGY_EVICT_USER_LATEST,
        AdmissionStrategyDto::GraceInstantStream => LABEL_ADMISSION_STRATEGY_GRACE_INSTANT_STREAM,
        AdmissionStrategyDto::GraceHoldStream => LABEL_ADMISSION_STRATEGY_GRACE_HOLD_STREAM,
    }
}

fn admission_strategy_label(translate: &YewI18n, strategy: AdmissionStrategyDto) -> String {
    t_safe(translate, admission_strategy_label_key(strategy)).unwrap_or_else(|| match strategy {
        AdmissionStrategyDto::EvictUserSameIpOldest => "Evict same-IP oldest stream".to_string(),
        AdmissionStrategyDto::EvictUserSameIpLatest => "Evict same-IP latest stream".to_string(),
        AdmissionStrategyDto::EvictUserOldest => "Evict user oldest stream".to_string(),
        AdmissionStrategyDto::EvictUserLatest => "Evict user latest stream".to_string(),
        AdmissionStrategyDto::GraceInstantStream => "Grace instant stream".to_string(),
        AdmissionStrategyDto::GraceHoldStream => "Grace hold stream".to_string(),
    })
}

fn is_grace_strategy(strategy: AdmissionStrategyDto) -> bool {
    matches!(strategy, AdmissionStrategyDto::GraceInstantStream | AdmissionStrategyDto::GraceHoldStream)
}

fn is_grace_strategy_tag(tag: &str) -> bool {
    let tag = tag.trim();
    tag.starts_with("grace_") || parse_admission_strategy_tag(tag).is_some_and(is_grace_strategy)
}

fn admission_strategy_tags(strategies: Option<&Vec<AdmissionStrategyDto>>) -> Option<Vec<String>> {
    strategies.map(|entries| entries.iter().map(|entry| admission_strategy_tag(*entry).to_string()).collect())
}

fn parse_admission_strategy_tags(tags: Option<&[String]>) -> Option<Vec<AdmissionStrategyDto>> {
    let tags = tags?;
    let mut parsed = Vec::new();
    for tag in tags {
        if let Some(strategy) = parse_admission_strategy_tag(tag) {
            if !parsed.contains(&strategy) {
                parsed.push(strategy);
            }
        }
    }
    Some(parsed)
}

fn filter_disabled_grace_strategy_tags(tags: Vec<String>, grace_period_millis: u64) -> Vec<String> {
    if grace_period_millis == 0 {
        tags.into_iter().filter(|tag| !is_grace_strategy_tag(tag)).collect()
    } else {
        tags
    }
}

fn filter_disabled_grace_strategies(
    strategies: Option<Vec<AdmissionStrategyDto>>,
    grace_period_millis: u64,
) -> Option<Vec<AdmissionStrategyDto>> {
    strategies.map(|entries| {
        if grace_period_millis == 0 {
            entries.into_iter().filter(|strategy| !is_grace_strategy(*strategy)).collect()
        } else {
            entries
        }
    })
}

fn admission_strategy_tag_label(translate: &YewI18n, tag: &str) -> String {
    parse_admission_strategy_tag(tag)
        .map(|strategy| admission_strategy_label(translate, strategy))
        .unwrap_or_else(|| tag.to_string())
}

fn legacy_admission_strategy_tags(stream: &StreamConfigDto) -> Vec<String> {
    if stream.grace_period_millis == 0 {
        Vec::new()
    } else {
        vec![admission_strategy_tag(if stream.grace_period_hold_stream {
            AdmissionStrategyDto::GraceHoldStream
        } else {
            AdmissionStrategyDto::GraceInstantStream
        })
        .to_string()]
    }
}

fn displayed_admission_strategy_tags(state: &AdmissionStrategiesDto, stream: &StreamConfigDto) -> Vec<String> {
    filter_disabled_grace_strategy_tags(
        state.strategies.clone().unwrap_or_else(|| {
            admission_strategy_tags(stream.admission_strategies.as_ref())
                .unwrap_or_else(|| legacy_admission_strategy_tags(stream))
        }),
        stream.grace_period_millis,
    )
}

fn available_admission_strategies(selected_tags: &[String], grace_period_millis: u64) -> Vec<AdmissionStrategyDto> {
    let has_grace = selected_tags.iter().filter_map(|tag| parse_admission_strategy_tag(tag)).any(is_grace_strategy);

    [
        AdmissionStrategyDto::EvictUserSameIpOldest,
        AdmissionStrategyDto::EvictUserSameIpLatest,
        AdmissionStrategyDto::EvictUserOldest,
        AdmissionStrategyDto::EvictUserLatest,
        AdmissionStrategyDto::GraceInstantStream,
        AdmissionStrategyDto::GraceHoldStream,
    ]
    .into_iter()
    .filter(|strategy| {
        let tag = admission_strategy_tag(*strategy);
        let grace_available = grace_period_millis > 0 || !is_grace_strategy(*strategy);
        !selected_tags.iter().any(|selected| selected == tag)
            && grace_available
            && (!has_grace || !is_grace_strategy(*strategy))
    })
    .collect()
}

fn add_admission_strategy_tag(current: &[String], strategy: AdmissionStrategyDto) -> Vec<String> {
    let mut next = current.to_vec();
    let tag = admission_strategy_tag(strategy).to_string();
    if next.iter().any(|selected| selected == &tag) {
        return next;
    }
    // When adding a same-IP eviction rule, insert it before any broader user-wide
    // rule (if present) so the backend ordering validation is satisfied.
    let is_narrower =
        matches!(strategy, AdmissionStrategyDto::EvictUserSameIpOldest | AdmissionStrategyDto::EvictUserSameIpLatest);
    if is_narrower {
        let broader_oldest = admission_strategy_tag(AdmissionStrategyDto::EvictUserOldest);
        let broader_latest = admission_strategy_tag(AdmissionStrategyDto::EvictUserLatest);

        let earliest_pos = next.iter().position(|t| t == broader_oldest || t == broader_latest);
        if let Some(pos) = earliest_pos {
            next.insert(pos, tag);
            return next;
        }
    }
    next.push(tag);
    next
}

fn remove_admission_strategy_tag(current: &[String], index: usize) -> Vec<String> {
    let mut next = current.to_vec();
    if index < next.len() {
        next.remove(index);
    }
    next
}

fn move_admission_strategy_tag(current: &[String], index: usize, delta: isize) -> Vec<String> {
    let mut next = current.to_vec();
    if let Some(target_index) = index.checked_add_signed(delta) {
        if index < next.len() && target_index < next.len() {
            next.swap(index, target_index);
            // Reject the move if it would create an invalid ordering (broader before narrower).
            let strategy_dtos: Vec<AdmissionStrategyDto> =
                next.iter().filter_map(|t| parse_admission_strategy_tag(t)).collect();
            if !shared::model::is_valid_admission_strategy_order(&strategy_dtos) {
                next.swap(index, target_index); // revert
            }
        }
    }
    next
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
        let stream_buffer_state = stream_buffer_state.clone();
        let failover_patterns_state = failover_patterns_state.clone();
        let admission_strategies_state = admission_strategies_state.clone();
        let stream_history_state = stream_history_state.clone();
        let qos_aggregation_state = qos_aggregation_state.clone();
        let last_emitted_form = last_emitted_form.clone();

        use_emit_mapped_option(
            (
                (
                    reverse_proxy_state.form.clone(),
                    disabled_header_state.form.clone(),
                    cache_state.form.clone(),
                    rate_limit_state.form.clone(),
                    resource_retry_state.form.clone(),
                    stream_state.form.clone(),
                    geoip_state.form.clone(),
                    stream_buffer_state.form.clone(),
                    failover_patterns_state.form.clone(),
                    admission_strategies_state.form.clone(),
                    stream_history_state.form.clone(),
                    qos_aggregation_state.form.clone(),
                ),
                (
                    reverse_proxy_state.modified,
                    disabled_header_state.modified,
                    cache_state.modified,
                    rate_limit_state.modified,
                    resource_retry_state.modified,
                    stream_state.modified,
                    geoip_state.modified,
                    stream_buffer_state.modified,
                    failover_patterns_state.modified,
                    admission_strategies_state.modified,
                    stream_history_state.modified,
                    qos_aggregation_state.modified,
                ),
            ),
            config_view_ctx.on_form_change.clone(),
            move |(
                (
                    rp,
                    disabled_header,
                    cache,
                    rl,
                    resource_retry,
                    stream,
                    geoip,
                    stream_buffer,
                    failover_patterns,
                    admission_strategies,
                    stream_history,
                    qos_aggregation,
                ),
                (
                    rp_modified,
                    disabled_header_modified,
                    cache_modified,
                    rl_modified,
                    resource_retry_modified,
                    stream_modified,
                    geoip_modified,
                    stream_buffer_modified,
                    failover_patterns_modified,
                    admission_strategies_modified,
                    stream_history_modified,
                    qos_aggregation_modified,
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
                <h1>{translate.t(LABEL_CACHE)}</h1>
                { config_field_bool!(cache_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field_optional!(cache_state.form, translate.t(LABEL_SIZE), size) }
                { config_field_optional!(cache_state.form, translate.t(LABEL_DIRECTORY), directory) }
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
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_GEOIP)}</h1>
                { edit_field_bool!(geoip_state, translate.t(LABEL_ENABLED), enabled, GeoIpConfigFormAction::Enabled) }
                { edit_field_text!(geoip_state, translate.t(LABEL_URL), url, GeoIpConfigFormAction::Url) }
            </Card>
        }
    };

    let render_cache_edit = || {
        html! {
          <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_CACHE)}</h1>
            { edit_field_bool!(cache_state, translate.t(LABEL_ENABLED), enabled, CacheConfigFormAction::Enabled) }
            { edit_field_text_option!(cache_state, translate.t(LABEL_SIZE), size, CacheConfigFormAction::Size) }
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
                                        hint="Move up"
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
                                        hint="Move down"
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
    fn admission_strategy_tags_roundtrip() {
        let tags = admission_strategy_tags(Some(&vec![
            AdmissionStrategyDto::EvictUserOldest,
            AdmissionStrategyDto::GraceHoldStream,
        ]))
        .unwrap_or_default();
        assert_eq!(
            parse_admission_strategy_tags(Some(&tags)),
            Some(vec![AdmissionStrategyDto::EvictUserOldest, AdmissionStrategyDto::GraceHoldStream,])
        );
    }

    #[test]
    fn admission_strategy_tags_roundtrip_evict_user_latest() {
        let tags = admission_strategy_tags(Some(&vec![AdmissionStrategyDto::EvictUserLatest])).unwrap_or_default();
        assert_eq!(parse_admission_strategy_tags(Some(&tags)), Some(vec![AdmissionStrategyDto::EvictUserLatest]));
    }

    #[test]
    fn invalid_admission_strategy_tags_are_ignored() {
        let tags = vec!["evict_user_latest".to_string(), "not-a-strategy".to_string(), "evict_user_latest".to_string()];
        assert_eq!(parse_admission_strategy_tags(Some(&tags)), Some(vec![AdmissionStrategyDto::EvictUserLatest]));
    }

    #[test]
    fn displayed_admission_strategies_fall_back_to_legacy_grace() {
        let state = AdmissionStrategiesDto::default();
        let stream = StreamConfigDto {
            grace_period_millis: 2_000,
            grace_period_hold_stream: true,
            ..StreamConfigDto::default()
        };

        assert_eq!(displayed_admission_strategy_tags(&state, &stream), vec!["grace_hold_stream".to_string()]);
    }

    #[test]
    fn available_admission_strategies_hide_second_grace_option() {
        let available = available_admission_strategies(&["grace_hold_stream".to_string()], 2_000);
        assert!(!available.contains(&AdmissionStrategyDto::GraceInstantStream));
        assert!(!available.contains(&AdmissionStrategyDto::GraceHoldStream));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserSameIpOldest));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserOldest));
    }

    #[test]
    fn available_admission_strategies_hide_grace_when_disabled() {
        let available = available_admission_strategies(&[], 0);
        assert!(!available.contains(&AdmissionStrategyDto::GraceInstantStream));
        assert!(!available.contains(&AdmissionStrategyDto::GraceHoldStream));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserSameIpOldest));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserSameIpLatest));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserOldest));
        assert!(available.contains(&AdmissionStrategyDto::EvictUserLatest));
    }

    #[test]
    fn displayed_admission_strategies_hide_disabled_grace_tags() {
        let state = AdmissionStrategiesDto { strategies: Some(vec!["grace_hold_stream".to_string()]) };
        let stream = StreamConfigDto { grace_period_millis: 0, ..StreamConfigDto::default() };

        assert_eq!(displayed_admission_strategy_tags(&state, &stream), Vec::<String>::new());
    }

    #[test]
    fn filtered_admission_strategies_drop_grace_when_disabled() {
        let parsed = parse_admission_strategy_tags(Some(&[
            "evict_user_same_ip_oldest".to_string(),
            "grace_hold_stream".to_string(),
        ]));

        assert_eq!(
            filter_disabled_grace_strategies(parsed, 0),
            Some(vec![AdmissionStrategyDto::EvictUserSameIpOldest])
        );
    }

    #[test]
    fn add_admission_strategy_enforces_narrower_before_broader() {
        let current = vec!["evict_user_oldest".to_string()];
        let new_tags = add_admission_strategy_tag(&current, "evict_user_same_ip_oldest");
        assert_eq!(new_tags, vec!["evict_user_same_ip_oldest".to_string(), "evict_user_oldest".to_string()]);
    }

    #[test]
    fn move_admission_strategy_reverts_invalid_order() {
        let current = vec!["evict_user_same_ip_oldest".to_string(), "evict_user_oldest".to_string()];
        // Attempt to move broader EvictUserOldest up before narrower EvictUserSameIpOldest
        let next = move_admission_strategy_tag(&current, 1, -1);
        assert_eq!(next, current);
    }
}
