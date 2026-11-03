//! Local information-panel positioning; menu focus and navigation remain independent.
use gloo_events::EventListener;
use gloo_render::{request_animation_frame, AnimationFrame};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::JsCast;
use web_sys::{Element, HtmlElement, KeyboardEvent};
use yew::prelude::*;

const GUTTER: f64 = 8.0;
const WIDTH: f64 = 320.0;
pub(super) const DETAILS_ID: &str = "tp-health-details";

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

#[derive(Debug, PartialEq)]
struct Placement {
    left: f64,
    top: f64,
    width: f64,
    max_height: f64,
    above: bool,
}

fn placement(anchor: Bounds, viewport: Bounds, natural_height: f64, rtl: bool) -> Option<Placement> {
    if anchor.right <= viewport.left
        || anchor.left >= viewport.right
        || anchor.bottom <= viewport.top
        || anchor.top >= viewport.bottom
    {
        return None;
    }
    let width = WIDTH.min(viewport.right - viewport.left - 2.0 * GUTTER);
    let below = viewport.bottom - GUTTER - anchor.bottom - GUTTER;
    let above = anchor.top - GUTTER - viewport.top - GUTTER;
    let flip = natural_height > below && (natural_height <= above || above > below);
    let max_height = if flip { above } else { below };
    if width <= 0.0 || max_height <= 0.0 {
        return None;
    }
    let end = if rtl { anchor.left } else { anchor.right - width };
    Some(Placement {
        left: end.clamp(viewport.left + GUTTER, viewport.right - GUTTER - width),
        top: if flip { anchor.top - GUTTER - natural_height.min(max_height) } else { anchor.bottom + GUTTER },
        width,
        max_height,
        above: flip,
    })
}

fn position(anchor: &Element, panel: &HtmlElement) -> Option<()> {
    let window = web_sys::window()?;
    let document = window.document()?;
    let root = document.document_element()?;
    let anchor_style = window.get_computed_style(anchor).ok()??;
    let rect = anchor.get_bounding_client_rect();
    if !anchor.is_connected()
        || rect.width() <= 0.0
        || rect.height() <= 0.0
        || anchor_style.get_property_value("visibility").ok()? != "visible"
    {
        return None;
    }
    let viewport = window.visual_viewport().map_or(
        Bounds { left: 0.0, top: 0.0, right: f64::from(root.client_width()), bottom: f64::from(root.client_height()) },
        |visual| Bounds {
            left: visual.offset_left(),
            top: visual.offset_top(),
            right: visual.offset_left() + visual.width(),
            bottom: visual.offset_top() + visual.height(),
        },
    );
    let style = panel.style();
    let width = WIDTH.min(viewport.right - viewport.left - 2.0 * GUTTER).max(0.0);
    style.set_property("--health-popover-width", &format!("{width}px")).ok()?;
    // Normalize client rectangles with the existing fixed element, as PopupMenu does.
    // Every probe and final write completes synchronously before the next paint.
    style.set_property("--health-popover-top", "0px").ok()?;
    style.set_property("--health-popover-left", "0px").ok()?;
    let probe = panel.get_bounding_client_rect();
    // Keep the current height constraint while measuring: expanding a scroll region
    // even temporarily would clamp its scrollTop back to zero on every frame.
    let border = probe.height() - f64::from(panel.client_height());
    let mut natural_height = f64::from(panel.scroll_height()) + border;
    if let Some(list) = panel
        .query_selector(".tp__health-banner__providers")
        .ok()?
        .and_then(|element| element.dyn_into::<HtmlElement>().ok())
    {
        natural_height = natural_height.max(probe.height() + f64::from(list.scroll_height() - list.client_height()));
    }
    let anchor = Bounds {
        left: rect.left() - probe.left(),
        right: rect.right() - probe.left(),
        top: rect.top() - probe.top(),
        bottom: rect.bottom() - probe.top(),
    };
    let rtl = anchor_style.get_property_value("direction").ok()? == "rtl";
    let result = placement(anchor, viewport, natural_height, rtl)?;
    style.set_property("--health-popover-top", &format!("{}px", result.top)).ok()?;
    style.set_property("--health-popover-left", &format!("{}px", result.left)).ok()?;
    style.set_property("--health-popover-max-height", &format!("{}px", result.max_height)).ok()?;
    panel.set_attribute("data-side", if result.above { "above" } else { "below" }).ok()?;
    if result.max_height < 180.0 {
        panel.set_attribute("data-compact", "true").ok()?;
    } else {
        panel.remove_attribute("data-compact").ok()?;
    }
    panel.set_attribute("data-positioned", "true").ok()?;
    Some(())
}

fn follow(frame: &Rc<RefCell<Option<AnimationFrame>>>, update: Rc<dyn Fn() -> bool>) {
    let weak = Rc::downgrade(frame);
    *frame.borrow_mut() = Some(request_animation_frame(move |_| {
        if update() {
            if let Some(frame) = weak.upgrade() {
                follow(&frame, update);
            }
        }
    }));
}

#[derive(Properties, PartialEq)]
pub(super) struct HealthPopoverProps {
    pub anchor: NodeRef,
    pub panel: NodeRef,
    pub label: String,
    pub on_close: Callback<()>,
    pub on_focus_change: Callback<bool>,
    pub onmouseenter: Callback<MouseEvent>,
    pub onmouseleave: Callback<MouseEvent>,
    pub onfocus: Callback<FocusEvent>,
    pub onblur: Callback<FocusEvent>,
    pub children: Children,
}

#[component]
pub(super) fn HealthPopover(props: &HealthPopoverProps) -> Html {
    {
        let anchor = props.anchor.clone();
        let panel = props.panel.clone();
        let close = props.on_close.clone();
        let focus_change = props.on_focus_change.clone();
        use_effect_with((), move |()| {
            let update: Rc<dyn Fn() -> bool> = Rc::new(move || {
                let valid = anchor.cast::<Element>().zip(panel.cast::<HtmlElement>()).is_some_and(|(anchor, panel)| {
                    if position(&anchor, &panel).is_some() {
                        // Removing a focused child need not dispatch blur. Reconcile
                        // against the DOM while this panel is already being followed.
                        let focused = web_sys::window()
                            .and_then(|window| window.document())
                            .and_then(|document| document.active_element())
                            .is_some_and(|active| anchor.contains(Some(&active)) || panel.contains(Some(&active)));
                        focus_change.emit(focused);
                        true
                    } else {
                        let _ = panel.remove_attribute("data-positioned");
                        false
                    }
                });
                if !valid {
                    close.emit(());
                }
                valid
            });
            let frame = Rc::new(RefCell::new(None));
            if update() {
                follow(&frame, update);
            }
            move || drop(frame.borrow_mut().take())
        });
    }
    {
        let close = props.on_close.clone();
        use_effect_with((), move |()| {
            let listeners = web_sys::window().map(|window| {
                let escape = close.clone();
                [
                    EventListener::new(&window, "keydown", move |event| {
                        if event.dyn_ref::<KeyboardEvent>().is_some_and(|key| key.key() == "Escape") {
                            escape.emit(());
                        }
                    }),
                    EventListener::new(&window, "hashchange", move |_| close.emit(())),
                ]
            });
            move || drop(listeners)
        });
    }
    // A fixed sibling preserves the natural badge -> details -> toolbar Tab order.
    // No portal/menu role, focus transfer or badge click handler surrounds this region.
    html! {
        <section id={DETAILS_ID} ref={props.panel.clone()} class="tp__health-banner__popover"
            role="region" aria-label={props.label.clone()} tabindex="0"
            onmouseenter={props.onmouseenter.clone()} onmouseleave={props.onmouseleave.clone()}
            onfocus={props.onfocus.clone()} onblur={props.onblur.clone()}>
            {for props.children.iter()}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ANCHOR: Bounds = Bounds { left: 1000.0, right: 1100.0, top: 15.0, bottom: 39.0 };
    const VIEW: Bounds = Bounds { left: 0.0, top: 0.0, right: 1280.0, bottom: 720.0 };

    #[test]
    fn health_banner_placement_aligns_logical_end_without_covering_anchor() {
        let ltr = placement(ANCHOR, VIEW, 145.0, false).unwrap();
        assert_eq!(ltr, Placement { left: 780.0, top: 47.0, width: 320.0, max_height: 665.0, above: false });
        let rtl = placement(Bounds { left: 140.0, right: 240.0, ..ANCHOR }, VIEW, 145.0, true).unwrap();
        assert_eq!(rtl.left, 140.0);
        assert_eq!(rtl.top, ltr.top);
    }

    #[test]
    fn health_banner_placement_clamps_narrow_and_offset_visual_viewports() {
        for width in [320.0, 390.0, 768.0, 1920.0] {
            for offset in [0.0, 50.0] {
                let viewport = Bounds { left: offset, right: offset + width, top: 80.0, bottom: 720.0 };
                let anchor = Bounds { left: offset + 75.0, right: offset + 170.0, top: 95.0, bottom: 119.0 };
                let placed = placement(anchor, viewport, 145.0, false).unwrap();
                assert_eq!(placed.width, WIDTH.min(width - 16.0));
                assert!(placed.left >= viewport.left + 8.0);
                assert!(placed.left + placed.width <= viewport.right - 8.0);
                assert_eq!(placed.top, 127.0, "visual offset is applied exactly once");
            }
        }
    }

    #[test]
    fn health_banner_placement_flips_or_limits_height_without_overlaying_anchor() {
        let bottom = Bounds { top: 600.0, bottom: 624.0, ..ANCHOR };
        let flipped = placement(bottom, VIEW, 200.0, false).unwrap();
        assert!(flipped.above);
        assert_eq!(flipped.top, 392.0);
        let tall = placement(ANCHOR, VIEW, 2000.0, false).unwrap();
        assert!(!tall.above);
        assert_eq!(tall.max_height, 665.0);
        assert_eq!(tall.top + tall.max_height, VIEW.bottom - 8.0);
    }

    #[test]
    fn health_banner_placement_rejects_invisible_anchors_or_no_available_space() {
        assert!(placement(Bounds { top: -40.0, bottom: -10.0, ..ANCHOR }, VIEW, 145.0, false).is_none());
        assert!(placement(ANCHOR, Bounds { bottom: 40.0, ..VIEW }, 145.0, false).is_none());
    }
}
