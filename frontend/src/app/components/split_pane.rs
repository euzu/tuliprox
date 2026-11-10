use crate::{
    app::components::AppIcon,
    i18n::use_translation,
    utils::{get_local_storage_item, resolve_css_length_px, set_local_storage_item},
};
use std::rc::Rc;
use web_sys::{HtmlElement, KeyboardEvent, MouseEvent, PointerEvent};
use yew::{
    classes, component, html, use_mut_ref, use_node_ref, use_state, AttrValue, Callback, Classes, Html, Properties,
};

const COLLAPSED_TOLERANCE_PX: f64 = 1.0;
const KEYBOARD_STEP_PX: f64 = 16.0;
const FALLBACK_MIN_PX: f64 = 128.0;
const FALLBACK_INITIAL_PX: f64 = 256.0;

/// Axis along which the two panes are laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SplitDirection {
    /// Panes side by side, the divider moves left and right.
    #[default]
    Horizontal,
    /// Panes stacked, the divider moves up and down.
    #[allow(dead_code)] // part of the component API, no view stacks panes yet
    Vertical,
}

/// One of the two panes of a [`SplitPane`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SplitSide {
    #[default]
    First,
    Second,
}

/// Two panes with a divider the user drags with the pointer or moves with the arrow keys.
/// Only one pane (`sized_pane`) has an explicit size; the other one takes the remaining space.
#[derive(Properties, PartialEq)]
pub struct SplitPaneProps {
    pub first: Html,
    pub second: Html,
    #[prop_or_default]
    pub direction: SplitDirection,
    /// The pane that owns the explicit size.
    #[prop_or_default]
    pub sized_pane: SplitSide,
    /// Start size of `sized_pane` as CSS length (`px` or `rem`).
    #[prop_or(AttrValue::Static("16rem"))]
    pub initial_size: AttrValue,
    /// Smallest size of `sized_pane`.
    #[prop_or(AttrValue::Static("8rem"))]
    pub min_size: AttrValue,
    #[prop_or_default]
    pub max_size: Option<AttrValue>,
    /// Space the other pane keeps at least while the divider is dragged.
    #[prop_or(AttrValue::Static("8rem"))]
    pub min_other_size: AttrValue,
    /// localStorage key to remember the size per viewer.
    #[prop_or_default]
    pub storage_key: Option<AttrValue>,
    /// Double click and Enter toggle `sized_pane` between `min_size` and its last size.
    #[prop_or_default]
    pub collapsible: bool,
    /// Start at `min_size` when no size is stored (for example on mobile).
    #[prop_or_default]
    pub start_collapsed: bool,
    /// Show only the second pane, without the divider.
    #[prop_or_default]
    pub first_hidden: bool,
    /// Stack the panes without divider below the mobile breakpoint.
    #[prop_or_default]
    pub stack_on_mobile: bool,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub first_class: Classes,
    #[prop_or_default]
    pub second_class: Classes,
    /// Called with the new size of `sized_pane` in px.
    #[prop_or_default]
    pub on_resize: Option<Callback<f64>>,
}

pub(crate) fn clamp_pane_size(size: f64, min: f64, max: Option<f64>) -> f64 {
    let size = size.max(min);
    max.map_or(size, |max| size.min(max.max(min)))
}

pub(crate) fn is_collapsed(size: f64, min: f64) -> bool { (size - min).abs() <= COLLAPSED_TOLERANCE_PX }

/// Size after a toggle: collapse an expanded pane, restore the last expanded size of a collapsed one.
pub(crate) fn toggle_pane_size(current: f64, last_expanded: f64, min: f64) -> f64 {
    if !is_collapsed(current, min) {
        min
    } else if is_collapsed(last_expanded, min) {
        min * 2.0
    } else {
        last_expanded
    }
}

/// Largest size of the sized pane so that the other pane keeps `min_other`.
pub(crate) fn max_for_container(container: f64, divider: f64, min_other: f64, max: Option<f64>) -> f64 {
    let available = (container - divider - min_other).max(0.0);
    max.map_or(available, |max| max.min(available))
}

/// Size of the sized pane after the divider moved by `delta` px along the pointer axis. `reversed`
/// is set when the first pane sits at the end of the axis (right-to-left text).
pub(crate) fn moved_size(start: f64, delta: f64, sized_pane: SplitSide, reversed: bool) -> f64 {
    if (sized_pane == SplitSide::Second) == reversed {
        start + delta
    } else {
        start - delta
    }
}

/// Pointer-axis step of an arrow key for the given direction.
pub(crate) fn key_delta(key: &str, direction: SplitDirection) -> Option<f64> {
    match (direction, key) {
        (SplitDirection::Horizontal, "ArrowLeft") | (SplitDirection::Vertical, "ArrowUp") => Some(-KEYBOARD_STEP_PX),
        (SplitDirection::Horizontal, "ArrowRight") | (SplitDirection::Vertical, "ArrowDown") => Some(KEYBOARD_STEP_PX),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Bounds {
    min: f64,
    max: Option<f64>,
    min_other: f64,
}

impl Bounds {
    fn clamp(self, size: f64) -> f64 { clamp_pane_size(size, self.min, self.max) }
}

#[derive(Clone, Copy)]
struct DragState {
    pointer: i32,
    size: f64,
    reversed: bool,
    max: Option<f64>,
}

fn is_rtl(element: &HtmlElement) -> bool {
    web_sys::window()
        .and_then(|win| win.get_computed_style(element).ok().flatten())
        .and_then(|style| style.get_property_value("direction").ok())
        .is_some_and(|direction| direction == "rtl")
}

fn extent(element: &HtmlElement, direction: SplitDirection) -> f64 {
    f64::from(match direction {
        SplitDirection::Horizontal => element.offset_width(),
        SplitDirection::Vertical => element.offset_height(),
    })
}

fn pointer_pos(e: &PointerEvent, direction: SplitDirection) -> i32 {
    match direction {
        SplitDirection::Horizontal => e.client_x(),
        SplitDirection::Vertical => e.client_y(),
    }
}

#[component]
pub fn SplitPane(props: &SplitPaneProps) -> Html {
    let translate = use_translation();
    let direction = props.direction;
    let sized_pane = props.sized_pane;
    let bounds = {
        let min = resolve_css_length_px(&props.min_size).unwrap_or(FALLBACK_MIN_PX);
        Bounds {
            min,
            max: props.max_size.as_deref().and_then(resolve_css_length_px),
            min_other: resolve_css_length_px(&props.min_other_size).unwrap_or(FALLBACK_MIN_PX),
        }
    };
    let size = {
        let storage_key = props.storage_key.clone();
        let initial_size = props.initial_size.clone();
        let start_collapsed = props.start_collapsed;
        use_state(move || {
            let stored = storage_key.as_deref().and_then(get_local_storage_item).and_then(|v| v.parse::<f64>().ok());
            let initial = stored.unwrap_or_else(|| {
                if start_collapsed {
                    bounds.min
                } else {
                    resolve_css_length_px(&initial_size).unwrap_or(FALLBACK_INITIAL_PX)
                }
            });
            bounds.clamp(initial)
        })
    };
    let last_expanded = {
        let initial_size = props.initial_size.clone();
        use_mut_ref(move || resolve_css_length_px(&initial_size).unwrap_or(FALLBACK_INITIAL_PX))
    };
    let dragging = use_state(|| false);
    let drag = use_mut_ref(|| None::<DragState>);
    let root_ref = use_node_ref();
    let divider_ref = use_node_ref();

    // Applies a new size; `persist` stores it (on release, not on every move).
    let set_size = {
        let size = size.clone();
        let last_expanded = last_expanded.clone();
        let storage_key = props.storage_key.clone();
        let on_resize = props.on_resize.clone();
        Rc::new(move |next: f64, max: Option<f64>, persist: bool| {
            let next = clamp_pane_size(next, bounds.min, max.or(bounds.max));
            if !is_collapsed(next, bounds.min) {
                *last_expanded.borrow_mut() = next;
            }
            size.set(next);
            if persist {
                if let Some(key) = storage_key.as_deref() {
                    set_local_storage_item(key, &format!("{next:.0}"));
                }
            }
            if let Some(callback) = on_resize.as_ref() {
                callback.emit(next);
            }
        })
    };

    // Largest size that still leaves `min_other` for the other pane in the current layout.
    let container_max = {
        let root_ref = root_ref.clone();
        let divider_ref = divider_ref.clone();
        Rc::new(move || {
            let root = root_ref.cast::<HtmlElement>()?;
            let divider = divider_ref.cast::<HtmlElement>()?;
            Some(max_for_container(extent(&root, direction), extent(&divider, direction), bounds.min_other, bounds.max))
        })
    };

    let on_pointer_down = {
        let drag = drag.clone();
        let size = size.clone();
        let dragging = dragging.clone();
        let root_ref = root_ref.clone();
        let divider_ref = divider_ref.clone();
        let container_max = container_max.clone();
        Callback::from(move |e: PointerEvent| {
            if e.button() != 0 {
                return;
            }
            e.prevent_default();
            // Pointer capture routes move/up to the divider even outside of it.
            if let Some(divider) = divider_ref.cast::<HtmlElement>() {
                let _ = divider.set_pointer_capture(e.pointer_id());
            }
            let reversed = direction == SplitDirection::Horizontal
                && root_ref.cast::<HtmlElement>().is_some_and(|root| is_rtl(&root));
            *drag.borrow_mut() =
                Some(DragState { pointer: pointer_pos(&e, direction), size: *size, reversed, max: container_max() });
            dragging.set(true);
        })
    };
    let on_pointer_move = {
        let drag = drag.clone();
        let set_size = set_size.clone();
        Callback::from(move |e: PointerEvent| {
            let Some(state) = *drag.borrow() else { return };
            let delta = f64::from(pointer_pos(&e, direction) - state.pointer);
            set_size(moved_size(state.size, delta, sized_pane, state.reversed), state.max, false);
        })
    };
    let on_pointer_up = {
        let drag = drag.clone();
        let set_size = set_size.clone();
        let size = size.clone();
        let dragging = dragging.clone();
        Callback::from(move |_e: PointerEvent| {
            let Some(state) = drag.borrow_mut().take() else { return };
            dragging.set(false);
            set_size(*size, state.max, true);
        })
    };
    let toggle = {
        let set_size = set_size.clone();
        let size = size.clone();
        let last_expanded = last_expanded.clone();
        let container_max = container_max.clone();
        let collapsible = props.collapsible;
        Rc::new(move || {
            if collapsible {
                set_size(toggle_pane_size(*size, *last_expanded.borrow(), bounds.min), container_max(), true);
            }
        })
    };
    let on_double_click = {
        let toggle = toggle.clone();
        Callback::from(move |_e: MouseEvent| toggle())
    };
    let on_key_down = {
        let set_size = set_size.clone();
        let size = size.clone();
        let root_ref = root_ref.clone();
        let container_max = container_max.clone();
        Callback::from(move |e: KeyboardEvent| {
            if let Some(delta) = key_delta(e.key().as_str(), direction) {
                e.prevent_default();
                let reversed = direction == SplitDirection::Horizontal
                    && root_ref.cast::<HtmlElement>().is_some_and(|root| is_rtl(&root));
                set_size(moved_size(*size, delta, sized_pane, reversed), container_max(), true);
            } else if matches!(e.key().as_str(), "Enter" | " ") {
                e.prevent_default();
                toggle();
            }
        })
    };

    let current = *size;
    let collapsed = props.collapsible && is_collapsed(current, bounds.min);
    let (orientation, divider_orientation) = match direction {
        SplitDirection::Horizontal => ("horizontal", "vertical"),
        SplitDirection::Vertical => ("vertical", "horizontal"),
    };
    let style = {
        // Always set, otherwise a nested split pane would inherit the limit of its parent.
        let max = bounds.max.map_or_else(|| "none".to_string(), |max| format!("{max:.0}px"));
        format!(
            "--tp-split-size:{current:.0}px;--tp-split-min:{:.0}px;--tp-split-min-other:{:.0}px;--tp-split-max:{max}",
            bounds.min, bounds.min_other
        )
    };
    let pane_class = |side: SplitSide, extra: &Classes| {
        classes!(
            "tp__split-pane__pane",
            if sized_pane == side { "tp__split-pane__pane--sized" } else { "tp__split-pane__pane--other" },
            extra.clone()
        )
    };

    let divider = html! {
        <div
            ref={divider_ref}
            class="tp__split-pane__divider"
            role="separator"
            aria-orientation={divider_orientation}
            aria-label={translate.t("LABEL.SPLIT_PANE_RESIZE")}
            aria-valuenow={format!("{current:.0}")}
            aria-valuemin={format!("{:.0}", bounds.min)}
            aria-valuemax={bounds.max.map(|max| format!("{max:.0}"))}
            tabindex="0"
            onpointerdown={on_pointer_down}
            onpointermove={on_pointer_move}
            onpointerup={on_pointer_up.clone()}
            onpointercancel={on_pointer_up}
            ondblclick={on_double_click}
            onkeydown={on_key_down}
        >
            <span class="tp__split-pane__grip"><AppIcon name="DragHandle"/></span>
        </div>
    };

    html! {
        <div
            ref={root_ref}
            class={classes!(
                "tp__split-pane",
                format!("tp__split-pane--{orientation}"),
                collapsed.then_some("tp__split-pane--collapsed"),
                (*dragging).then_some("tp__split-pane--dragging"),
                props.first_hidden.then_some("tp__split-pane--first-hidden"),
                props.stack_on_mobile.then_some("tp__split-pane--stack-mobile"),
                props.class.clone()
            )}
            style={style}
        >
            <div class={pane_class(SplitSide::First, &props.first_class)}>{ props.first.clone() }</div>
            { divider }
            <div class={pane_class(SplitSide::Second, &props.second_class)}>{ props.second.clone() }</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_pane_size, is_collapsed, key_delta, max_for_container, moved_size, toggle_pane_size, SplitDirection,
        SplitSide,
    };

    #[test]
    fn clamp_respects_min_and_max() {
        assert_eq!(clamp_pane_size(10.0, 72.0, Some(400.0)), 72.0);
        assert_eq!(clamp_pane_size(500.0, 72.0, Some(400.0)), 400.0);
        assert_eq!(clamp_pane_size(200.0, 72.0, None), 200.0);
        // a max below the min size never wins
        assert_eq!(clamp_pane_size(200.0, 72.0, Some(50.0)), 72.0);
    }

    #[test]
    fn toggle_switches_between_collapsed_and_last_expanded() {
        assert_eq!(toggle_pane_size(250.0, 250.0, 72.0), 72.0);
        assert_eq!(toggle_pane_size(72.0, 250.0, 72.0), 250.0);
    }

    #[test]
    fn toggle_from_collapsed_without_history_uses_double_min() {
        assert_eq!(toggle_pane_size(72.0, 72.0, 72.0), 144.0);
    }

    #[test]
    fn collapsed_detection_has_tolerance() {
        assert!(is_collapsed(72.4, 72.0));
        assert!(!is_collapsed(80.0, 72.0));
    }

    #[test]
    fn container_max_keeps_room_for_other_pane() {
        assert_eq!(max_for_container(1000.0, 8.0, 200.0, None), 792.0);
        assert_eq!(max_for_container(1000.0, 8.0, 200.0, Some(400.0)), 400.0);
        assert_eq!(max_for_container(1000.0, 8.0, 200.0, Some(900.0)), 792.0);
        // a container smaller than the other pane's minimum never yields a negative size
        assert_eq!(max_for_container(100.0, 8.0, 200.0, None), 0.0);
    }

    #[test]
    fn moved_size_follows_pointer_for_first_pane() {
        assert_eq!(moved_size(200.0, 30.0, SplitSide::First, false), 230.0);
        assert_eq!(moved_size(200.0, -30.0, SplitSide::First, false), 170.0);
    }

    #[test]
    fn moved_size_inverts_for_second_pane() {
        assert_eq!(moved_size(200.0, 30.0, SplitSide::Second, false), 170.0);
    }

    #[test]
    fn moved_size_inverts_for_reversed_axis() {
        assert_eq!(moved_size(200.0, 30.0, SplitSide::First, true), 170.0);
        assert_eq!(moved_size(200.0, 30.0, SplitSide::Second, true), 230.0);
    }

    #[test]
    fn key_delta_matches_direction() {
        assert_eq!(key_delta("ArrowRight", SplitDirection::Horizontal), Some(16.0));
        assert_eq!(key_delta("ArrowLeft", SplitDirection::Horizontal), Some(-16.0));
        assert_eq!(key_delta("ArrowDown", SplitDirection::Vertical), Some(16.0));
        assert_eq!(key_delta("ArrowUp", SplitDirection::Vertical), Some(-16.0));
        assert_eq!(key_delta("ArrowUp", SplitDirection::Horizontal), None);
        assert_eq!(key_delta("ArrowLeft", SplitDirection::Vertical), None);
    }
}
