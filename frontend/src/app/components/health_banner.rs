use crate::{
    app::context::{ConfigContext, StatusContext},
    hooks::use_websocket_status,
    i18n::use_translation,
    model::ViewType,
    utils::set_location_hash,
};
use gloo_timers::callback::{Interval, Timeout};
use std::{cell::RefCell, rc::Rc};

mod popover;
use popover::{HealthPopover, DETAILS_ID};
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

#[derive(Default)]
struct OpenConditions {
    badge_hover: bool,
    panel_hover: bool,
    focus: bool,
    dismissed: bool,
}

impl OpenConditions {
    fn wants_open(&self) -> bool { !self.dismissed && (self.badge_hover || self.panel_hover || self.focus) }
}

type CloseTimer = Rc<RefCell<Option<Timeout>>>;

fn schedule_close(timer: &CloseTimer, conditions: Rc<RefCell<OpenConditions>>, open: UseStateHandle<bool>) {
    *timer.borrow_mut() = Some(Timeout::new(180, move || {
        if !conditions.borrow().wants_open() {
            open.set(false);
        }
    }));
}

#[component]
pub fn HealthBanner() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>();
    let config_ctx = use_context::<ConfigContext>();
    let banner_ref = use_node_ref();
    let panel_ref = use_node_ref();
    let conditions = use_mut_ref(OpenConditions::default);
    let close_timer = use_mut_ref(|| None::<Timeout>);
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

    let dismiss = {
        let open = popover_open.clone();
        let conditions = conditions.clone();
        let timer = close_timer.clone();
        let panel = panel_ref.clone();
        let badge = banner_ref.clone();
        Callback::from(move |()| {
            let focused_badge = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.active_element())
                .zip(badge.cast::<web_sys::Element>())
                .is_some_and(|(active, badge)| badge.contains(Some(&active)));
            {
                let mut conditions = conditions.borrow_mut();
                conditions.dismissed = true;
                conditions.panel_hover = false;
                conditions.focus = focused_badge;
            }
            timer.borrow_mut().take();
            if let Some(panel) = panel.cast::<web_sys::Element>() {
                let _ = panel.remove_attribute("data-positioned");
            }
            open.set(false);
        })
    };
    let onclick = {
        let dismiss = dismiss.clone();
        Callback::from(move |_| {
            dismiss.emit(());
            set_location_hash(ViewType::Stats.as_str());
        })
    };
    let onkeydown = {
        let dismiss = dismiss.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" || e.key() == " " {
                e.prevent_default();
                dismiss.emit(());
                set_location_hash(ViewType::Stats.as_str());
            }
        })
    };
    let hover = |panel: bool, enter: bool| {
        let conditions = conditions.clone();
        let timer = close_timer.clone();
        let open = popover_open.clone();
        Callback::from(move |_: MouseEvent| {
            {
                let mut conditions = conditions.borrow_mut();
                if panel {
                    conditions.panel_hover = enter;
                } else {
                    conditions.badge_hover = enter;
                }
                if enter {
                    conditions.dismissed = false;
                }
            }
            timer.borrow_mut().take();
            if enter {
                open.set(true);
            } else {
                schedule_close(&timer, conditions.clone(), open.clone());
            }
        })
    };
    let onfocus = {
        let conditions = conditions.clone();
        let timer = close_timer.clone();
        let open = popover_open.clone();
        Callback::from(move |_: FocusEvent| {
            let mut conditions = conditions.borrow_mut();
            conditions.focus = true;
            conditions.dismissed = false;
            timer.borrow_mut().take();
            open.set(true);
        })
    };
    let onblur = {
        let conditions = conditions.clone();
        let timer = close_timer.clone();
        let open = popover_open.clone();
        let badge = banner_ref.clone();
        let panel = panel_ref.clone();
        Callback::from(move |event: FocusEvent| {
            use wasm_bindgen::JsCast;
            let target = event.related_target().and_then(|target| target.dyn_into::<web_sys::Node>().ok());
            conditions.borrow_mut().focus = target.as_ref().is_some_and(|target| {
                [badge.cast::<web_sys::Element>(), panel.cast::<web_sys::Element>()]
                    .into_iter()
                    .flatten()
                    .any(|element| element.contains(Some(target)))
            });
            schedule_close(&timer, conditions.clone(), open.clone());
        })
    };
    let on_focus_change = {
        let conditions = conditions.clone();
        let timer = close_timer.clone();
        let open = popover_open.clone();
        Callback::from(move |focused: bool| {
            {
                let mut conditions = conditions.borrow_mut();
                if conditions.focus == focused {
                    return;
                }
                conditions.focus = focused;
            }
            timer.borrow_mut().take();
            if !focused {
                schedule_close(&timer, conditions.clone(), open.clone());
            }
        })
    };
    {
        let timer = close_timer.clone();
        use_effect_with((), move |()| move || drop(timer.borrow_mut().take()));
    }

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

    let providers_summary = if !has_status {
        translate.t("LABEL.HEALTH_UNKNOWN")
    } else if provider_rows.is_empty() {
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
                    <progress class={classes!("tp__health-banner__bar", row.signal.modifier())}
                        aria-hidden="true" max="1" value={ratio.to_string()} />
                    <bdi class="tp__health-banner__provider-value" dir="ltr">{ value }</bdi>
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
        <>
        <div
            ref={banner_ref.clone()}
            class={classes!("tp__health-banner", health.modifier())}
            role="button"
            aria-label={aria_label}
            aria-expanded={popover_open.to_string()}
            aria-controls={(*popover_open).then_some(DETAILS_ID)}
            aria-describedby={(*popover_open).then_some(DETAILS_ID)}
            tabindex="0"
            onclick={onclick}
            onkeydown={onkeydown}
            onmouseenter={hover(false, true)}
            onmouseleave={hover(false, false)}
            onfocus={onfocus.clone()}
            onblur={onblur.clone()}
        >
            <span class="tp__health-banner__dot" aria-hidden="true" />
            <span class="tp__health-banner__label" role="status" aria-live="polite" aria-atomic="true">{ label }</span>
        </div>
        if *popover_open {
            <HealthPopover anchor={banner_ref} panel={panel_ref} label={translate.t("LABEL.HEALTH_BANNER")}
                on_close={dismiss} on_focus_change={on_focus_change}
                onmouseenter={hover(true, true)} onmouseleave={hover(true, false)}
                onfocus={onfocus} onblur={onblur}>
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
                if !provider_rows.is_empty() {
                    <div class="tp__health-banner__providers" tabindex="0" role="region"
                        aria-label={translate.t("LABEL.HEALTH_PROVIDERS")}>
                        { provider_detail }
                    </div>
                }
            </HealthPopover>
        }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_health, compute_saturated, Health, OpenConditions};
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };

    #[test]
    fn health_banner_escape_stays_dismissed_until_a_new_entry_or_focus() {
        let mut conditions = OpenConditions { badge_hover: true, focus: true, ..Default::default() };
        assert!(conditions.wants_open());
        conditions.dismissed = true;
        assert!(!conditions.wants_open());
        conditions.badge_hover = false;
        conditions.focus = false;
        assert!(!conditions.wants_open());
        conditions.panel_hover = true;
        conditions.dismissed = false;
        assert!(conditions.wants_open());
    }

    #[test]
    fn health_banner_reading_or_focus_keeps_panel_open_after_leaving_badge() {
        let mut conditions = OpenConditions { panel_hover: true, ..Default::default() };
        assert!(conditions.wants_open());
        conditions.panel_hover = false;
        conditions.focus = true;
        assert!(conditions.wants_open());
        conditions.focus = false;
        assert!(!conditions.wants_open());
    }

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
