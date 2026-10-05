use crate::utils::{get_local_storage_item, resolve_css_length_px, set_local_storage_item};
use std::rc::Rc;
use web_sys::{HtmlElement, KeyboardEvent, MouseEvent, PointerEvent};
use yew::{
    classes, component, html, use_mut_ref, use_node_ref, use_state, AttrValue, Callback, Children, Classes, Html,
    Properties,
};

const SHRUNK_TOLERANCE_PX: f64 = 1.0;
const KEYBOARD_STEP_PX: f64 = 16.0;
const FALLBACK_SHRINK_PX: f64 = 64.0;
const FALLBACK_DEFAULT_PX: f64 = 256.0;

/// A panel whose width the user changes with a drag handle on its right edge. It never gets
/// narrower than `shrink_width`, so part of its content stays visible when shrunk.
#[derive(Properties, PartialEq)]
pub struct HorizontalShrinkPanelProps {
    #[prop_or_default]
    pub children: Children,
    /// Smallest width, as CSS length (`px` or `rem`).
    pub shrink_width: AttrValue,
    #[prop_or(AttrValue::Static("16rem"))]
    pub default_width: AttrValue,
    #[prop_or_default]
    pub max_width: Option<AttrValue>,
    /// localStorage key to remember the width per viewer.
    #[prop_or_default]
    pub storage_key: Option<AttrValue>,
    /// Start shrunk when no width is stored (for example on mobile).
    #[prop_or_default]
    pub start_shrunk: bool,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub on_width_change: Option<Callback<f64>>,
}

pub(crate) fn clamp_panel_width(width: f64, shrink: f64, max: Option<f64>) -> f64 {
    let width = width.max(shrink);
    max.map_or(width, |max| width.min(max.max(shrink)))
}

pub(crate) fn is_shrunk(width: f64, shrink: f64) -> bool { (width - shrink).abs() <= SHRUNK_TOLERANCE_PX }

/// Width after a toggle: shrink an expanded panel, restore the last expanded width of a shrunk one.
pub(crate) fn toggle_panel_width(current: f64, last_expanded: f64, shrink: f64) -> f64 {
    if !is_shrunk(current, shrink) {
        shrink
    } else if is_shrunk(last_expanded, shrink) {
        shrink * 2.0
    } else {
        last_expanded
    }
}

#[derive(Clone, Copy)]
struct Bounds {
    shrink: f64,
    max: Option<f64>,
}

impl Bounds {
    fn clamp(self, width: f64) -> f64 { clamp_panel_width(width, self.shrink, self.max) }
}

#[derive(Clone, Copy)]
struct DragState {
    pointer_x: i32,
    width: f64,
}

#[component]
pub fn HorizontalShrinkPanel(props: &HorizontalShrinkPanelProps) -> Html {
    let bounds = {
        let shrink = resolve_css_length_px(&props.shrink_width).unwrap_or(FALLBACK_SHRINK_PX);
        Bounds { shrink, max: props.max_width.as_deref().and_then(resolve_css_length_px) }
    };
    let width = {
        let storage_key = props.storage_key.clone();
        let default_width = props.default_width.clone();
        let start_shrunk = props.start_shrunk;
        use_state(move || {
            let stored = storage_key.as_deref().and_then(get_local_storage_item).and_then(|v| v.parse::<f64>().ok());
            let initial = stored.unwrap_or_else(|| {
                if start_shrunk {
                    bounds.shrink
                } else {
                    resolve_css_length_px(&default_width).unwrap_or(FALLBACK_DEFAULT_PX)
                }
            });
            bounds.clamp(initial)
        })
    };
    let last_expanded = {
        let default_width = props.default_width.clone();
        use_mut_ref(move || resolve_css_length_px(&default_width).unwrap_or(FALLBACK_DEFAULT_PX))
    };
    let drag = use_mut_ref(|| None::<DragState>);
    let handle_ref = use_node_ref();

    // Applies a new width; `persist` stores it (on release, not on every move).
    let set_width = {
        let width = width.clone();
        let last_expanded = last_expanded.clone();
        let storage_key = props.storage_key.clone();
        let on_width_change = props.on_width_change.clone();
        Rc::new(move |next: f64, persist: bool| {
            let next = bounds.clamp(next);
            if !is_shrunk(next, bounds.shrink) {
                *last_expanded.borrow_mut() = next;
            }
            width.set(next);
            if persist {
                if let Some(key) = storage_key.as_deref() {
                    set_local_storage_item(key, &format!("{next:.0}"));
                }
            }
            if let Some(callback) = on_width_change.as_ref() {
                callback.emit(next);
            }
        })
    };

    let on_pointer_down = {
        let drag = drag.clone();
        let width = width.clone();
        let handle_ref = handle_ref.clone();
        Callback::from(move |e: PointerEvent| {
            if e.button() != 0 {
                return;
            }
            e.prevent_default();
            // Pointer capture routes move/up to the handle even outside of it.
            if let Some(handle) = handle_ref.cast::<HtmlElement>() {
                let _ = handle.set_pointer_capture(e.pointer_id());
            }
            *drag.borrow_mut() = Some(DragState { pointer_x: e.client_x(), width: *width });
        })
    };
    let on_pointer_move = {
        let drag = drag.clone();
        let set_width = set_width.clone();
        Callback::from(move |e: PointerEvent| {
            let Some(state) = *drag.borrow() else { return };
            set_width(state.width + f64::from(e.client_x() - state.pointer_x), false);
        })
    };
    let on_pointer_up = {
        let drag = drag.clone();
        let set_width = set_width.clone();
        let width = width.clone();
        Callback::from(move |_e: PointerEvent| {
            if drag.borrow_mut().take().is_some() {
                set_width(*width, true);
            }
        })
    };
    let toggle = {
        let set_width = set_width.clone();
        let width = width.clone();
        let last_expanded = last_expanded.clone();
        Rc::new(move || set_width(toggle_panel_width(*width, *last_expanded.borrow(), bounds.shrink), true))
    };
    let on_double_click = {
        let toggle = toggle.clone();
        Callback::from(move |_e: MouseEvent| toggle())
    };
    let on_key_down = {
        let set_width = set_width.clone();
        let width = width.clone();
        Callback::from(move |e: KeyboardEvent| match e.key().as_str() {
            "ArrowLeft" => {
                e.prevent_default();
                set_width(*width - KEYBOARD_STEP_PX, true);
            }
            "ArrowRight" => {
                e.prevent_default();
                set_width(*width + KEYBOARD_STEP_PX, true);
            }
            "Enter" | " " => {
                e.prevent_default();
                toggle();
            }
            _ => {}
        })
    };

    let current = *width;
    let shrunk = is_shrunk(current, bounds.shrink);
    html! {
        <div
            class={classes!("tp__shrink-panel", shrunk.then_some("tp__shrink-panel--shrunk"), props.class.clone())}
            style={format!("width:{current:.0}px;min-width:{current:.0}px;max-width:{current:.0}px")}
        >
            <div class="tp__shrink-panel__content">{ props.children.clone() }</div>
            <div
                ref={handle_ref}
                class="tp__shrink-panel__handle"
                role="separator"
                aria-orientation="vertical"
                aria-valuenow={format!("{current:.0}")}
                tabindex="0"
                onpointerdown={on_pointer_down}
                onpointermove={on_pointer_move}
                onpointerup={on_pointer_up.clone()}
                onpointercancel={on_pointer_up}
                ondblclick={on_double_click}
                onkeydown={on_key_down}
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_panel_width, is_shrunk, toggle_panel_width};

    #[test]
    fn clamp_respects_shrink_and_max() {
        assert_eq!(clamp_panel_width(10.0, 72.0, Some(400.0)), 72.0);
        assert_eq!(clamp_panel_width(500.0, 72.0, Some(400.0)), 400.0);
        assert_eq!(clamp_panel_width(200.0, 72.0, None), 200.0);
        // a max below the shrink width never wins
        assert_eq!(clamp_panel_width(200.0, 72.0, Some(50.0)), 72.0);
    }

    #[test]
    fn toggle_switches_between_shrunk_and_last_expanded() {
        assert_eq!(toggle_panel_width(250.0, 250.0, 72.0), 72.0);
        assert_eq!(toggle_panel_width(72.0, 250.0, 72.0), 250.0);
    }

    #[test]
    fn toggle_from_shrunk_without_history_uses_double_shrink() {
        assert_eq!(toggle_panel_width(72.0, 72.0, 72.0), 144.0);
    }

    #[test]
    fn shrunk_detection_has_tolerance() {
        assert!(is_shrunk(72.4, 72.0));
        assert!(!is_shrunk(80.0, 72.0));
    }
}
