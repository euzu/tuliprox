use crate::{
    app::context::{ConfigContext, StatusContext},
    hooks::use_websocket_status,
    i18n::use_translation,
    model::ViewType,
    utils::set_location_hash,
};
use gloo_timers::callback::Interval;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use yew::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
}

impl Health {
    fn modifier(self) -> &'static str {
        match self {
            Health::Unknown => "tp__health-banner--unknown",
            Health::Healthy => "tp__health-banner--healthy",
            Health::Degraded => "tp__health-banner--degraded",
            Health::Unhealthy => "tp__health-banner--unhealthy",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    Ok,
    Warn,
    Bad,
}

impl Signal {
    fn modifier(self) -> &'static str {
        match self {
            Signal::Ok => "tp__health-banner__signal--ok",
            Signal::Warn => "tp__health-banner__signal--warn",
            Signal::Bad => "tp__health-banner__signal--bad",
        }
    }
}

// A provider is considered "near capacity" (amber) at or above this usage ratio.
const CAPACITY_WARN_RATIO: f64 = 0.8;

struct ProviderRow {
    name: Arc<str>,
    current: usize,
    max: u16,
    signal: Signal,
}

/// Slots are built from the configured enabled members, not from the live
/// connection rows: an idle fallback still offers capacity and must keep its
/// group from being marked saturated.
fn compute_saturated(
    capacity_lookup: &HashMap<Arc<str>, u16>,
    connections: Option<&BTreeMap<Arc<str>, usize>>,
    group_lookup: &HashMap<Arc<str>, Arc<str>>,
) -> bool {
    shared::model::provider_saturation::is_exhausted(
        capacity_lookup.iter().map(|(name, max)| shared::model::provider_saturation::ProviderSlot {
            name: name.clone(),
            max_connections: *max,
            current: connections.and_then(|map| map.get(name)).copied().unwrap_or(0),
        }),
        group_lookup,
    )
}

fn compute_health(ws_connected: Option<bool>, has_status: bool, backend_ok: bool, saturated: bool) -> Health {
    if ws_connected == Some(false) {
        Health::Unhealthy
    } else if ws_connected.is_none() || !has_status {
        Health::Unknown
    } else if !backend_ok {
        Health::Unhealthy
    } else if saturated {
        Health::Degraded
    } else {
        Health::Healthy
    }
}

fn build_provider_capacity_lookup(config_ctx: &Option<ConfigContext>) -> HashMap<Arc<str>, u16> {
    config_ctx
        .as_ref()
        .and_then(|ctx| ctx.config.as_ref())
        .map(|cfg| {
            let mut map = HashMap::new();
            for input in &cfg.sources.inputs {
                if !input.enabled {
                    continue;
                }
                map.insert(input.name.clone(), input.max_connections);
                if let Some(aliases) = &input.aliases {
                    for alias in aliases {
                        if alias.enabled {
                            map.insert(alias.name.clone(), alias.max_connections);
                        }
                    }
                }
            }
            map
        })
        .unwrap_or_default()
}

fn build_input_group_lookup(config_ctx: &Option<ConfigContext>) -> HashMap<Arc<str>, Arc<str>> {
    config_ctx
        .as_ref()
        .and_then(|ctx| ctx.config.as_ref())
        .map(|cfg| shared::model::provider_saturation::build_group_lookup(&cfg.sources.inputs))
        .unwrap_or_default()
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn open_popover_callback<E: 'static>(
    popover_open: UseStateHandle<bool>,
    now_ms: UseStateHandle<f64>,
    update_popover_pos: Callback<()>,
) -> Callback<E> {
    Callback::from(move |_| {
        popover_open.set(true);
        now_ms.set(js_sys::Date::now());
        update_popover_pos.emit(());
    })
}

#[component]
pub fn HealthBanner() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>();
    let config_ctx = use_context::<ConfigContext>();
    let banner_ref = use_node_ref();
    let popover_pos = use_state(|| (0.0_f64, 0.0_f64));
    let popover_open = use_state(|| false);
    let now_ms = use_state(js_sys::Date::now);

    let ws_connected = use_websocket_status();

    let status = status_ctx.as_ref().and_then(|ctx| ctx.status.clone());
    let has_status = status.is_some();
    let ws_status = Some(*ws_connected);
    let max_lookup = {
        let config_ctx = config_ctx.clone();
        use_memo(config_ctx, build_provider_capacity_lookup)
    };
    let group_lookup = {
        let config_ctx = config_ctx.clone();
        use_memo(config_ctx, build_input_group_lookup)
    };

    let mut provider_rows: Vec<ProviderRow> = Vec::new();
    let backend_ok = match &status {
        Some(stats) => {
            if let Some(map) = &stats.active_provider_connections {
                for (name, current) in map {
                    let max = max_lookup.get(name.as_ref()).copied().unwrap_or(0);
                    if *current == 0 && max == 0 {
                        continue;
                    }
                    let signal = if max == 0 {
                        Signal::Ok
                    } else {
                        let ratio = *current as f64 / f64::from(max);
                        if ratio >= 1.0 {
                            Signal::Bad
                        } else if ratio >= CAPACITY_WARN_RATIO {
                            Signal::Warn
                        } else {
                            Signal::Ok
                        }
                    };
                    provider_rows.push(ProviderRow { name: name.clone(), current: *current, max, signal });
                }
            }
            stats.status == "ok"
        }
        None => true,
    };
    let saturated = compute_saturated(
        &max_lookup,
        status.as_ref().and_then(|stats| stats.active_provider_connections.as_ref()),
        &group_lookup,
    );

    let health = compute_health(ws_status, has_status, backend_ok, saturated);

    let last_change = use_state(js_sys::Date::now);
    {
        let last_change = last_change.clone();
        let now_ms = now_ms.clone();
        use_effect_with(health, move |_| {
            let now = js_sys::Date::now();
            last_change.set(now);
            now_ms.set(now);
            || ()
        });
    }
    {
        let popover_open = popover_open.clone();
        let now_ms = now_ms.clone();
        use_effect_with(*popover_open, move |is_open| {
            let interval = if *is_open {
                now_ms.set(js_sys::Date::now());
                Some(Interval::new(1000, move || now_ms.set(js_sys::Date::now())))
            } else {
                None
            };
            move || drop(interval)
        });
    }
    let elapsed_secs = (((*now_ms) - *last_change) / 1000.0).max(0.0) as u64;

    let label = match health {
        Health::Healthy => translate.t("LABEL.HEALTH_HEALTHY"),
        Health::Degraded => translate.t("LABEL.HEALTH_DEGRADED"),
        Health::Unhealthy => translate.t("LABEL.HEALTH_UNHEALTHY"),
        Health::Unknown => translate.t("LABEL.HEALTH_UNKNOWN"),
    };
    let aria_label = format!("{}: {label}", translate.t("LABEL.HEALTH_BANNER"));

    let onclick = Callback::from(|_| set_location_hash(ViewType::Stats.as_str()));
    let onkeydown = Callback::from(|e: KeyboardEvent| {
        if e.key() == "Enter" || e.key() == " " {
            e.prevent_default();
            set_location_hash(ViewType::Stats.as_str());
        }
    });

    let update_popover_pos = {
        let banner_ref = banner_ref.clone();
        let popover_pos = popover_pos.clone();
        Callback::from(move |(): ()| {
            if let Some(el) = banner_ref.cast::<web_sys::Element>() {
                let rect = el.get_bounding_client_rect();
                let viewport_w = web_sys::window()
                    .and_then(|w| w.inner_width().ok())
                    .and_then(|v| v.as_f64())
                    .unwrap_or(rect.right());
                let top = rect.bottom() + 8.0;
                let right = (viewport_w - rect.right()).max(0.0);
                popover_pos.set((top, right));
            }
        })
    };
    let onmouseenter =
        open_popover_callback::<MouseEvent>(popover_open.clone(), now_ms.clone(), update_popover_pos.clone());
    let onfocus = open_popover_callback::<FocusEvent>(popover_open.clone(), now_ms.clone(), update_popover_pos.clone());
    let onmouseleave = {
        let popover_open = popover_open.clone();
        Callback::from(move |_: MouseEvent| popover_open.set(false))
    };
    let onblur = {
        let popover_open = popover_open.clone();
        Callback::from(move |_: FocusEvent| popover_open.set(false))
    };
    let (popover_top, popover_right) = *popover_pos;
    let popover_style = format!("top:{popover_top:.0}px;right:{popover_right:.0}px");

    // Popover signal rows.
    let ws_signal = match ws_status {
        Some(true) => Some(Signal::Ok),
        Some(false) => Some(Signal::Bad),
        None => None,
    };
    let ws_value = match ws_status {
        Some(true) => translate.t("LABEL.HEALTH_CONNECTED"),
        Some(false) => translate.t("LABEL.HEALTH_DISCONNECTED"),
        None => translate.t("LABEL.HEALTH_UNKNOWN"),
    };

    let providers_summary = if provider_rows.is_empty() {
        translate.t("LABEL.HEALTH_PROVIDERS_NONE")
    } else {
        let warn_count = provider_rows.iter().filter(|p| p.signal != Signal::Ok).count();
        if warn_count == 0 {
            translate.t("LABEL.HEALTH_PROVIDERS_OK")
        } else {
            format!("{warn_count}/{}", provider_rows.len())
        }
    };

    let provider_detail = provider_rows
        .iter()
        .map(|row| {
            let ratio = if row.max == 0 { 0.0 } else { (row.current as f64 / f64::from(row.max)).clamp(0.0, 1.0) };
            let value =
                if row.max == 0 { format!("{} / ∞", row.current) } else { format!("{} / {}", row.current, row.max) };
            html! {
                <div class="tp__health-banner__provider">
                    <span class={classes!("tp__health-banner__signal", row.signal.modifier())} aria-hidden="true" />
                    <span class="tp__health-banner__provider-name">{ row.name.to_string() }</span>
                    <span class="tp__health-banner__bar" aria-hidden="true">
                        <span class="tp__health-banner__bar-fill" style={format!("width:{:.0}%", ratio * 100.0)} />
                    </span>
                    <span class="tp__health-banner__provider-value">{ value }</span>
                </div>
            }
        })
        .collect::<Html>();

    let backend_row = has_status.then(|| {
        let backend_signal = if backend_ok { Signal::Ok } else { Signal::Bad };
        let backend_value = status.as_ref().map_or_else(|| "n/a".to_string(), |s| s.status.clone());
        html! {
            <div class="tp__health-banner__row">
                <span class={classes!("tp__health-banner__signal", backend_signal.modifier())} aria-hidden="true" />
                <span class="tp__health-banner__row-label">{ translate.t("LABEL.HEALTH_BACKEND") }</span>
                <span class="tp__health-banner__row-value">{ backend_value }</span>
            </div>
        }
    });

    html! {
        <div
            ref={banner_ref}
            class={classes!("tp__health-banner", health.modifier())}
            role="button"
            aria-label={aria_label}
            tabindex="0"
            onclick={onclick}
            onkeydown={onkeydown}
            onmouseenter={onmouseenter}
            onmouseleave={onmouseleave}
            onfocus={onfocus}
            onblur={onblur}
        >
            <span class="tp__health-banner__dot" aria-hidden="true" />
            <span class="tp__health-banner__label" role="status" aria-live="polite" aria-atomic="true">{ label }</span>
            <div class="tp__health-banner__popover" role="presentation" style={popover_style}>
                <div class="tp__health-banner__popover-head">
                    <span class="tp__health-banner__popover-title">{ translate.t("LABEL.HEALTH_BANNER") }</span>
                    <span class="tp__health-banner__popover-since">{ format_elapsed(elapsed_secs) }</span>
                </div>
                <div class="tp__health-banner__row">
                    <span class={classes!("tp__health-banner__signal", ws_signal.map(Signal::modifier))} aria-hidden="true" />
                    <span class="tp__health-banner__row-label">{ translate.t("LABEL.HEALTH_WEBSOCKET") }</span>
                    <span class="tp__health-banner__row-value">{ ws_value }</span>
                </div>
                { backend_row.unwrap_or_default() }
                <div class="tp__health-banner__row">
                    <span class="tp__health-banner__row-label">{ translate.t("LABEL.HEALTH_PROVIDERS") }</span>
                    <span class="tp__health-banner__row-value">{ providers_summary }</span>
                </div>
                { provider_detail }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_health, compute_saturated, Health};
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };

    #[test]
    fn compute_health_starts_unknown_without_websocket_status() {
        assert_eq!(compute_health(None, true, true, false), Health::Unknown);
    }

    #[test]
    fn compute_health_marks_websocket_disconnect_unhealthy() {
        assert_eq!(compute_health(Some(false), true, true, false), Health::Unhealthy);
    }

    #[test]
    fn compute_health_marks_backend_down_unhealthy() {
        assert_eq!(compute_health(Some(true), true, false, false), Health::Unhealthy);
    }

    #[test]
    fn compute_health_marks_all_groups_exhausted_degraded() {
        assert_eq!(compute_health(Some(true), true, true, true), Health::Degraded);
    }

    #[test]
    fn compute_health_primary_busy_with_available_fallback_is_healthy() {
        assert_eq!(compute_health(Some(true), true, true, false), Health::Healthy);
    }

    #[test]
    fn idle_fallback_capacity_keeps_group_from_saturating() {
        let capacity = HashMap::from([(Arc::from("main"), 1u16), (Arc::from("fallback"), 5u16)]);
        let groups =
            HashMap::from([(Arc::from("main"), Arc::from("main")), (Arc::from("fallback"), Arc::from("main"))]);
        // Only "main" carries a connection; "fallback" is idle and absent from
        // the live connection map.
        let mut connections = BTreeMap::new();
        connections.insert(Arc::from("main"), 1usize);
        assert!(!compute_saturated(&capacity, Some(&connections), &groups));
        connections.insert(Arc::from("fallback"), 5usize);
        assert!(compute_saturated(&capacity, Some(&connections), &groups));
    }
}
