#![allow(clippy::large_enum_variant)]

use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::{CacheConfigDto, GeoIpConfigDto, RateLimitConfigDto, ResourceRetryConfigDto, ReverseProxyConfigDto, ReverseProxyDisabledHeaderConfigDto, StreamBufferConfigDto, StreamConfigDto};
use shared::utils::{default_secret, format_float_localized};
use crate::app::context::ConfigContext;
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::config::config_page::{ConfigForm, LABEL_REVERSE_PROXY_CONFIG};
use crate::app::components::{Card};
use crate::{config_field, config_field_bool, config_field_custom, config_field_hide, config_field_optional,
            edit_field_bool, edit_field_list, edit_field_number, edit_field_number_f64,
            edit_field_number_u64, edit_field_number_usize, edit_field_text, edit_field_text_option, generate_form_reducer};

const LABEL_CACHE: &str = "LABEL.CACHE";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_SIZE: &str = "LABEL.SIZE";
const LABEL_DIRECTORY: &str = "LABEL.DIRECTORY";

const LABEL_STREAM: &str = "LABEL.STREAM";
const LABEL_RETRY: &str = "LABEL.RETRY";
const LABEL_THROTTLE: &str = "LABEL.THROTTLE";
const LABEL_GRACE_PERIOD_MILLIS: &str = "LABEL.GRACE_PERIOD_MILLIS";
const LABEL_GRACE_PERIOD_TIMEOUT_SECS: &str = "LABEL.GRACE_PERIOD_TIMEOUT_SECS";
const LABEL_THROTTLE_KBPS: &str = "LABEL.THROTTLE_KBPS";
const LABEL_STREAM_BUFFER: &str = "LABEL.STREAM_BUFFER";
const LABEL_BUFFER_ENABLED: &str = "LABEL.BUFFER_ENABLED";
const LABEL_BUFFER_SIZE: &str = "LABEL.BUFFER_SIZE";

const LABEL_RATE_LIMIT: &str = "LABEL.RATE_LIMIT";
const LABEL_PERIOD_MILLIS: &str = "LABEL.PERIOD_MILLIS";
const LABEL_BURST_SIZE: &str = "LABEL.BURST_SIZE";
const LABEL_SHARED_BURST_BUFFER_MB: &str = "LABEL.SHARED_BURST_BUFFER_BYTES";

const LABEL_SETTINGS: &str = "LABEL.SETTINGS";
const LABEL_RESOURCE_REWRITE_DISABLED: &str = "LABEL.RESOURCE_REWRITE_DISABLED";
const LABEL_REWRITE_SECRET: &str = "LABEL.REWRITE_SECRET";
const LABEL_RESOURCE_RETRY: &str = "LABEL.RESOURCE_RETRY";
const LABEL_MAX_ATTEMPTS: &str = "LABEL.MAX_ATTEMPTS";
const LABEL_BACKOFF_MILLIS: &str = "LABEL.BACKOFF_MILLIS";
const LABEL_BACKOFF_MULTIPLIER: &str = "LABEL.BACKOFF_MULTIPLIER";
const LABEL_DISABLED_HEADER: &str = "LABEL.DISABLED_HEADER";
const LABEL_REFERER_HEADER: &str = "LABEL.REFERER_HEADER";
const LABEL_X_HEADER: &str = "LABEL.X_HEADER";
const LABEL_CF_HEADER: &str = "LABEL.CF_HEADER";
const LABEL_CUSTOM_HEADERS: &str = "LABEL.CUSTOM_HEADERS";
const LABEL_ADD_HEADER: &str = "LABEL.ADD_HEADER";
const LABEL_GEOIP: &str = "LABEL.GEOIP";
const LABEL_URL: &str = "LABEL.URL";

generate_form_reducer!(
    state: CacheConfigFormState { form: CacheConfigDto },
    action_name: CacheConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Size => size: Option<String>,
        Dir => dir: Option<String>,
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

generate_form_reducer!(
    state: StreamConfigFormState { form: StreamConfigDto },
    action_name: StreamConfigFormAction,
    fields {
        Retry => retry: bool,
        Throttle => throttle: Option<String>,
        GracePeriodMillis => grace_period_millis: u64,
        GracePeriodTimeoutSecs => grace_period_timeout_secs: u64,
        ThrottleKbps => throttle_kbps: u64,
        SharedBurstBufferMb => shared_burst_buffer_mb: u64,
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

#[function_component]
pub fn ReverseProxyConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let reverse_proxy_state: UseReducerHandle<ReverseProxyConfigFormState> = use_reducer(|| {
        ReverseProxyConfigFormState { form: ReverseProxyConfigDto { rewrite_secret: default_secret(), ..Default::default() }, modified: false }
    });
    let disabled_header_state: UseReducerHandle<ReverseProxyDisabledHeaderConfigFormState> = use_reducer(|| {
        ReverseProxyDisabledHeaderConfigFormState { form: ReverseProxyDisabledHeaderConfigDto::default(), modified: false }
    });
    let cache_state: UseReducerHandle<CacheConfigFormState> = use_reducer(|| {
        CacheConfigFormState { form: CacheConfigDto::default(), modified: false }
    });
    let rate_limit_state: UseReducerHandle<RateLimitConfigFormState> = use_reducer(|| {
        RateLimitConfigFormState { form: RateLimitConfigDto::default(), modified: false }
    });
    let resource_retry_state: UseReducerHandle<ResourceRetryConfigFormState> = use_reducer(|| {
        ResourceRetryConfigFormState { form: ResourceRetryConfigDto::default(), modified: false }
    });
    let stream_state: UseReducerHandle<StreamConfigFormState> = use_reducer(|| {
        StreamConfigFormState { form: StreamConfigDto::default(), modified: false }
    });

    let geoip_state: UseReducerHandle<GeoIpConfigFormState> = use_reducer(|| {
        GeoIpConfigFormState { form: GeoIpConfigDto::default(), modified: false }
    });

    let stream_buffer_state: UseReducerHandle<StreamBufferConfigFormState> = use_reducer(|| {
        StreamBufferConfigFormState { form: StreamBufferConfigDto::default(), modified: false }
    });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let reverse_proxy_state = reverse_proxy_state.clone();
        let disabled_header_state = disabled_header_state.clone();
        let cache_state = cache_state.clone();
        let rate_limit_state = rate_limit_state.clone();
        let resource_retry_state = resource_retry_state.clone();
        let stream_state = stream_state.clone();
        let geoip_state = geoip_state.clone();
        let stream_buffer_state = stream_buffer_state.clone();

        use_effect_with(
            (
                reverse_proxy_state,
                disabled_header_state,
                cache_state,
                rate_limit_state,
                resource_retry_state,
                stream_state,
                geoip_state,
                stream_buffer_state,
            ),
            move |(rp, disabled_header, cache, rl, resource_retry, stream, geoip, stream_buffer)| {
                let mut form = rp.form.clone();
                let mut stream_form = stream.form.clone();
                stream_form.buffer = if stream_buffer.form.is_empty() {
                    None
                } else {
                    Some(stream_buffer.form.clone())
                };

                form.cache = Some(cache.form.clone());
                form.rate_limit = Some(rl.form.clone());
                form.resource_retry = Some(resource_retry.form.clone());
                form.stream = Some(stream_form);
                form.geoip = Some(geoip.form.clone());
                form.disabled_header = if disabled_header.form.is_empty() {
                    None
                } else {
                    Some(disabled_header.form.clone())
                };

                let modified = rp.modified
                    || disabled_header.modified
                    || cache.modified
                    || rl.modified
                    || resource_retry.modified
                    || stream.modified
                    || geoip.modified
                    || stream_buffer.modified;
                on_form_change.emit(ConfigForm::ReverseProxy(modified, form));
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

        let reverse_proxy_cfg = config_ctx.config.as_ref().and_then(|c| c.config.reverse_proxy.clone());
        use_effect_with((reverse_proxy_cfg, config_view_ctx.edit_mode.clone()), move |(cfg, _mode)| {
            if let Some(rp) = cfg {
                reverse_proxy_state.dispatch(ReverseProxyConfigFormAction::SetAll((*rp).clone()));
                disabled_header_state.dispatch(ReverseProxyDisabledHeaderConfigFormAction::SetAll(rp.disabled_header.as_ref().map_or_else(ReverseProxyDisabledHeaderConfigDto::default, |d| d.clone())));
                cache_state.dispatch(CacheConfigFormAction::SetAll(rp.cache.as_ref().map_or_else(CacheConfigDto::default, |c| c.clone())));
                rate_limit_state.dispatch(RateLimitConfigFormAction::SetAll(rp.rate_limit.as_ref().map_or_else(RateLimitConfigDto::default, |rl| rl.clone())));
                resource_retry_state.dispatch(ResourceRetryConfigFormAction::SetAll(rp.resource_retry.as_ref().map_or_else(ResourceRetryConfigDto::default, |rr| rr.clone())));
                stream_state.dispatch(StreamConfigFormAction::SetAll(rp.stream.as_ref().map_or_else(StreamConfigDto::default, |s| s.clone())));
                geoip_state.dispatch(GeoIpConfigFormAction::SetAll(rp.geoip.as_ref().map_or_else(GeoIpConfigDto::default, |s| s.clone())));
                stream_buffer_state.dispatch(StreamBufferConfigFormAction::SetAll(rp.stream.as_ref().and_then(|s| s.buffer.clone()).unwrap_or_default()));
            } else {
                reverse_proxy_state.dispatch(ReverseProxyConfigFormAction::SetAll(ReverseProxyConfigDto::default()));
                disabled_header_state.dispatch(ReverseProxyDisabledHeaderConfigFormAction::SetAll(ReverseProxyDisabledHeaderConfigDto::default()));
                cache_state.dispatch(CacheConfigFormAction::SetAll(CacheConfigDto::default()));
                rate_limit_state.dispatch(RateLimitConfigFormAction::SetAll(RateLimitConfigDto::default()));
                resource_retry_state.dispatch(ResourceRetryConfigFormAction::SetAll(ResourceRetryConfigDto::default()));
                stream_state.dispatch(StreamConfigFormAction::SetAll(StreamConfigDto::default()));
                geoip_state.dispatch(GeoIpConfigFormAction::SetAll(GeoIpConfigDto::default()));
                stream_buffer_state.dispatch(StreamBufferConfigFormAction::SetAll(StreamBufferConfigDto::default()));
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
                { config_field_optional!(cache_state.form, translate.t(LABEL_DIRECTORY), dir) }
            </Card>
        }
    };
    let render_stream = || {
        html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_STREAM)}</h1>
                { config_field_bool!(stream_state.form, translate.t(LABEL_RETRY), retry) }
                { config_field_optional!(stream_state.form, translate.t(LABEL_THROTTLE), throttle) }
                { config_field!(stream_state.form, translate.t(LABEL_GRACE_PERIOD_MILLIS), grace_period_millis) }
                { config_field!(stream_state.form, translate.t(LABEL_GRACE_PERIOD_TIMEOUT_SECS), grace_period_timeout_secs) }
                { config_field!(stream_state.form, translate.t(LABEL_THROTTLE_KBPS), throttle_kbps) }
                { config_field!(stream_state.form, translate.t(LABEL_SHARED_BURST_BUFFER_MB), shared_burst_buffer_mb) }
            </Card>
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

    let render_geoip_edit = || html ! {
        <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_GEOIP)}</h1>
            { edit_field_bool!(geoip_state, translate.t(LABEL_ENABLED), enabled, GeoIpConfigFormAction::Enabled) }
            { edit_field_text!(geoip_state, translate.t(LABEL_URL), url, GeoIpConfigFormAction::Url) }
        </Card>
    };

    let render_cache_edit = || html! {
      <Card class="tp__config-view__card">
        <h1>{translate.t(LABEL_CACHE)}</h1>
        { edit_field_bool!(cache_state, translate.t(LABEL_ENABLED), enabled, CacheConfigFormAction::Enabled) }
        { edit_field_text_option!(cache_state, translate.t(LABEL_SIZE), size, CacheConfigFormAction::Size) }
        { edit_field_text_option!(cache_state, translate.t(LABEL_DIRECTORY), dir, CacheConfigFormAction::Dir) }
      </Card>
    };

    let render_rate_limit_edit = || html! {
        <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_RATE_LIMIT)}</h1>
            { edit_field_bool!(rate_limit_state, translate.t(LABEL_ENABLED), enabled, RateLimitConfigFormAction::Enabled) }
            { edit_field_number_u64!(rate_limit_state, translate.t(LABEL_PERIOD_MILLIS), period_millis, RateLimitConfigFormAction::PeriodMillis) }
            { edit_field_number!(rate_limit_state, translate.t(LABEL_BURST_SIZE), burst_size, RateLimitConfigFormAction::BurstSize) }
        </Card>
    };

    let render_stream_edit = || html! {
        <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_STREAM)}</h1>
            { edit_field_bool!(stream_state, translate.t(LABEL_RETRY), retry, StreamConfigFormAction::Retry) }
            { edit_field_text_option!(stream_state, translate.t(LABEL_THROTTLE), throttle, StreamConfigFormAction::Throttle) }
            { edit_field_number_u64!(stream_state, translate.t(LABEL_GRACE_PERIOD_MILLIS), grace_period_millis, StreamConfigFormAction::GracePeriodMillis) }
            { edit_field_number_u64!(stream_state, translate.t(LABEL_GRACE_PERIOD_TIMEOUT_SECS), grace_period_timeout_secs, StreamConfigFormAction::GracePeriodTimeoutSecs) }
            { edit_field_number_u64!(stream_state, translate.t(LABEL_THROTTLE_KBPS), throttle_kbps, StreamConfigFormAction::ThrottleKbps) }
            { edit_field_number_u64!(stream_state, translate.t(LABEL_SHARED_BURST_BUFFER_MB), shared_burst_buffer_mb, StreamConfigFormAction::SharedBurstBufferMb) }
        </Card>
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


    let render_view_mode = || {
        html! {
            <div class="tp__reverse-proxy-config-view__body tp__config-view-page__body">
                { render_settings_view() }
                { render_disabled_header_view() }
                { render_geoip() }
                { render_cache() }
                { render_resource_retry_view() }
                { render_rate_limit() }
                { render_stream() }
                { render_stream_buffer() }
            </div>
        }
    };

    let render_edit_mode = || html! {
        <div class="tp__reverse-proxy-config-view__body tp__config-view-page__body">
            { render_settings_edit() }
            { render_disabled_header_edit() }
            { render_geoip_edit() }
            { render_cache_edit() }
            { render_resource_retry_edit() }
            { render_rate_limit_edit() }
            { render_stream_edit() }
            { render_stream_buffer_edit() }
        </div>
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
