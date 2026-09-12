use crate::hooks::use_key_down;
use std::{cell::Cell, rc::Rc};
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, Event, HtmlElement, KeyboardEvent, MouseEvent};
use yew::{create_portal, prelude::*};

const VIEWPORT_GUTTER_PX: f64 = 8.0;

/// Opt-in popup width policy; ordinary menus keep their content-sized trigger anchor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupMenuWidth {
    #[default]
    Content,
    MatchAnchor,
}

/// Popup placement policy; anchored menus can opt out of viewport flipping and clamping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupMenuPlacement {
    #[default]
    Adaptive,
    /// Keep the popup's top-right corner attached to the anchor's bottom-right corner.
    BottomEnd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PopupAnchorRect {
    top: f64,
    bottom: f64,
    left: f64,
    width: f64,
}

impl PopupAnchorRect {
    fn in_fixed_coordinates(self, fixed_origin: PopupPosition) -> Self {
        Self {
            top: self.top - fixed_origin.top,
            bottom: self.bottom - fixed_origin.top,
            left: self.left - fixed_origin.left,
            ..self
        }
    }
}

#[derive(Clone, Copy)]
struct PopupSize {
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PopupPosition {
    top: f64,
    left: f64,
}

fn popup_anchor_rect(anchor: &web_sys::Element) -> Option<PopupAnchorRect> {
    let rect = anchor.get_bounding_client_rect();
    (anchor.is_connected() && rect.width() > 0.0 && rect.height() > 0.0).then_some(PopupAnchorRect {
        top: rect.top(),
        bottom: rect.bottom(),
        left: rect.left(),
        width: rect.width(),
    })
}

fn resolve_popup_anchor(
    width: PopupMenuWidth,
    trigger: PopupAnchorRect,
    field: Option<PopupAnchorRect>,
) -> Option<PopupAnchorRect> {
    match width {
        PopupMenuWidth::MatchAnchor => field,
        PopupMenuWidth::Content => Some(trigger),
    }
}

// Use layout bounds so pinch zoom does not detach or shrink the menu relative to its field.
fn popup_position(
    anchor: PopupAnchorRect,
    popup: PopupSize,
    viewport: PopupSize,
    gutter: f64,
    placement: PopupMenuPlacement,
) -> PopupPosition {
    match placement {
        PopupMenuPlacement::Adaptive => {
            let max_bottom = viewport.height - gutter;
            let max_right = viewport.width - gutter;
            let mut top = anchor.bottom + gutter;
            let mut left = anchor.left;

            if left + popup.width > max_right {
                left = max_right - popup.width;
            }
            if top + popup.height > max_bottom {
                let top_above = anchor.top - popup.height - gutter;
                top = if top_above >= gutter { top_above } else { max_bottom - popup.height };
            }

            PopupPosition { top: top.max(gutter), left: left.max(gutter) }
        }
        PopupMenuPlacement::BottomEnd => {
            PopupPosition { top: anchor.bottom + gutter, left: anchor.left + anchor.width - popup.width }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PopupPositionEvent {
    Scroll,
    Resize,
    VisualViewportScroll,
    VisualViewportResize,
}

impl PopupPositionEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Scroll | Self::VisualViewportScroll => "scroll",
            Self::Resize | Self::VisualViewportResize => "resize",
        }
    }

    fn capture(self) -> bool {
        // Element scroll events do not bubble, including the input-card list.
        matches!(self, Self::Scroll)
    }

    fn target(self) -> Option<web_sys::EventTarget> {
        let window = window()?;
        match self {
            Self::Scroll | Self::Resize => Some(window.into()),
            Self::VisualViewportScroll | Self::VisualViewportResize => window.visual_viewport().map(Into::into),
        }
    }
}

// The effect owns this cleanup, both when closing and when unmounting.
fn track_popup_position<C: FnOnce()>(
    reposition: Rc<dyn Fn() -> Option<()>>,
    on_close: Callback<()>,
    mut listen: impl FnMut(PopupPositionEvent, Rc<dyn Fn()>) -> C,
) -> impl FnOnce() {
    let active = Rc::new(Cell::new(true));
    let update: Rc<dyn Fn()> = {
        let active = Rc::clone(&active);
        Rc::new(move || {
            if active.get() && reposition().is_none() {
                active.set(false);
                on_close.emit(());
            }
        })
    };
    update();
    let listeners = active.get().then(|| {
        [
            PopupPositionEvent::Scroll,
            PopupPositionEvent::Resize,
            PopupPositionEvent::VisualViewportScroll,
            PopupPositionEvent::VisualViewportResize,
        ]
        .map(|event| listen(event, Rc::clone(&update)))
    });

    move || {
        active.set(false);
        if let Some(listeners) = listeners {
            for remove in listeners {
                remove();
            }
        }
    }
}

fn next_menu_item_index(key: &str, current: Option<u32>, count: u32) -> Option<u32> {
    if count == 0 {
        return None;
    }
    match key {
        "ArrowDown" => Some(current.map_or(0, |index| (index + 1) % count)),
        "ArrowUp" => Some(current.map_or(count - 1, |index| (index + count - 1) % count)),
        "Home" => Some(0),
        "End" => Some(count - 1),
        _ => None,
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct PopupMenuProps {
    pub is_open: bool,
    pub anchor_ref: Option<web_sys::Element>,
    #[prop_or_default]
    pub width: PopupMenuWidth,
    #[prop_or_default]
    pub placement: PopupMenuPlacement,
    /// Full field whose width and edges the opt-in menu should match on every screen size.
    #[prop_or_default]
    pub width_anchor_ref: Option<NodeRef>,
    #[prop_or_default]
    pub on_close: Callback<()>,
    /// ARIA role of the option list, e.g. `listbox` for select-style popups.
    #[prop_or_else(|| "menu".to_string())]
    pub list_role: String,
    pub children: Children,
}

#[component]
pub fn PopupMenu(props: &PopupMenuProps) -> Html {
    let popup_ref = use_node_ref();

    // Calculate popup position relative to anchor and keep inside viewport
    let style = {
        let is_open = props.is_open;
        let anchor_ref = props.anchor_ref.clone();
        use_memo((is_open, anchor_ref.clone()), move |(is_open, anchor_ref)| {
            if !*is_open || anchor_ref.is_none() {
                return "hidden".to_string();
            }
            String::new()
        })
    };

    {
        let popup_ref = popup_ref.clone();
        let anchor_ref = props.anchor_ref.clone();
        let width = props.width;
        let placement = props.placement;
        let width_anchor_ref = props.width_anchor_ref.clone();
        let on_close = props.on_close.clone();
        use_effect_with(
            (props.is_open, anchor_ref, popup_ref.clone(), width, placement, width_anchor_ref),
            move |(is_open, anchor_ref, popup_ref, width, placement, width_anchor_ref)| {
                let cleanup = is_open.then(|| {
                    let reposition = {
                        let anchor_ref = anchor_ref.clone();
                        let popup_ref = popup_ref.clone();
                        let width = *width;
                        let placement = *placement;
                        let width_anchor_ref = width_anchor_ref.clone();
                        let first_position = Cell::new(true);
                        Rc::new(move || {
                            let popup = popup_ref.cast::<HtmlElement>()?;
                            let root = window()?.document()?.document_element()?;
                            let layout_viewport = PopupSize {
                                width: f64::from(root.client_width()),
                                height: f64::from(root.client_height()),
                            };
                            let trigger = popup_anchor_rect(anchor_ref.as_ref()?)?;
                            let field = if width == PopupMenuWidth::MatchAnchor {
                                width_anchor_ref
                                    .as_ref()
                                    .and_then(NodeRef::cast::<web_sys::Element>)
                                    .and_then(|field| popup_anchor_rect(&field))
                            } else {
                                None
                            };
                            let anchor = resolve_popup_anchor(width, trigger, field)?;
                            // Set width before measuring height: wrapping can affect flip/clamping.
                            let _ = popup.style().set_property("--popup-anchor-width", &format!("{}px", anchor.width));
                            // Safari can report client rectangles relative to the visual viewport.
                            // Measure the existing fixed popup at (0, 0) to normalize the anchor without
                            // browser sniffing. All writes complete in this callback before painting.
                            let _ = popup.style().set_property("--popup-top", "0px");
                            let _ = popup.style().set_property("--popup-left", "0px");
                            let popup_rect = popup.get_bounding_client_rect();
                            let fixed_origin = PopupPosition { top: popup_rect.top(), left: popup_rect.left() };
                            let position = popup_position(
                                anchor.in_fixed_coordinates(fixed_origin),
                                PopupSize { width: popup_rect.width(), height: popup_rect.height() },
                                layout_viewport,
                                VIEWPORT_GUTTER_PX,
                                placement,
                            );
                            let _ = popup.style().set_property("--popup-top", &format!("{}px", position.top));
                            let _ = popup.style().set_property("--popup-left", &format!("{}px", position.left));
                            let _ = popup.style().remove_property("visibility");

                            // Repositioning must not steal focus from the current menu item.
                            if first_position.replace(false) {
                                if let Ok(Some(first)) = popup.query_selector("button") {
                                    if let Ok(button) = first.dyn_into::<HtmlElement>() {
                                        let _ = button.focus();
                                    }
                                }
                            }
                            Some(())
                        })
                    };
                    let close_unpositioned = {
                        let popup_ref = popup_ref.clone();
                        Callback::from(move |()| {
                            if let Some(popup) = popup_ref.cast::<HtmlElement>() {
                                let _ = popup.style().set_property("visibility", "hidden");
                            }
                            on_close.emit(());
                        })
                    };
                    track_popup_position(reposition, close_unpositioned, |event, update| {
                        let target = event.target();
                        let handler = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_| update()));
                        if let Some(target) = target.as_ref() {
                            let _ = target.add_event_listener_with_callback_and_bool(
                                event.name(),
                                handler.as_ref().unchecked_ref(),
                                event.capture(),
                            );
                        }
                        move || {
                            if let Some(target) = target {
                                let _ = target.remove_event_listener_with_callback_and_bool(
                                    event.name(),
                                    handler.as_ref().unchecked_ref(),
                                    event.capture(),
                                );
                            }
                        }
                    })
                });
                move || {
                    if let Some(cleanup) = cleanup {
                        cleanup();
                    }
                }
            },
        );
    }

    // Close popup when clicking outside of it
    {
        let popup_ref = popup_ref.clone();
        let on_close = props.on_close.clone();
        use_effect_with(props.is_open, move |is_open| {
            let browser_window = web_sys::window();
            let handler = if *is_open {
                let handler = Closure::wrap(Box::new(move |event: MouseEvent| {
                    if let Some(popup) = popup_ref.cast::<HtmlElement>() {
                        // Cast to Node so clicks on SVG elements outside the popup also close it
                        if let Some(target) = event.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()) {
                            if !popup.contains(Some(&target)) {
                                on_close.emit(());
                            }
                        }
                    }
                }) as Box<dyn FnMut(_)>);

                if let Some(win) = browser_window.as_ref() {
                    let _ = win.add_event_listener_with_callback("mousedown", handler.as_ref().unchecked_ref());
                }
                Some(handler)
            } else {
                None
            };

            // Cleanup-Funktion
            move || {
                if let Some(handler) = handler {
                    if let Some(win) = browser_window.as_ref() {
                        let _ = win.remove_event_listener_with_callback("mousedown", handler.as_ref().unchecked_ref());
                    }
                }
            }
        });
    }

    {
        let is_open = props.is_open;
        let on_close = props.on_close.clone();
        let popup_ref = popup_ref.clone();
        use_key_down((is_open, on_close.clone()), move |event: &KeyboardEvent| {
            if !is_open {
                return;
            }
            let key = event.key();
            if key == "Escape" {
                on_close.emit(());
                return;
            }
            let Some(popup) = popup_ref.cast::<HtmlElement>() else {
                return;
            };
            let Ok(items) = popup.query_selector_all("button") else {
                return;
            };
            let count = items.length();
            if count == 0 {
                return;
            }
            let active = window().and_then(|w| w.document()).and_then(|d| d.active_element());
            let current = active.and_then(|active| {
                (0..count).find(|i| {
                    items
                        .item(*i)
                        .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                        .is_some_and(|el| el == active)
                })
            });
            let next = next_menu_item_index(&key, current, count);
            if let Some(idx) = next {
                event.prevent_default();
                if let Some(item) = items.item(idx).and_then(|node| node.dyn_into::<HtmlElement>().ok()) {
                    let _ = item.focus();
                }
            }
        });
    }

    let popup = html! {
        <div class={classes!(
            "tp__popup-menu",
            (props.width == PopupMenuWidth::MatchAnchor).then_some("tp__popup-menu--match-anchor"),
            (*style).clone()
        )} ref={popup_ref}>
            <ul role={props.list_role.clone()}>
                { for props.children.iter().map(|child| html! { <li role="none">{child.clone()}</li> }) }
            </ul>
        </div>
    };

    if let Some(document) = window().and_then(|win| win.document()) {
        if let Some(body) = document.body() {
            return create_portal(popup, body.into());
        }
    }

    popup
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const POPUP_SIZE: PopupSize = PopupSize { width: 160.0, height: 120.0 };
    const VIEWPORT: PopupSize = PopupSize { width: 1024.0, height: 768.0 };

    struct TestListener {
        id: usize,
        event: PopupPositionEvent,
        callback: Rc<dyn Fn()>,
    }

    #[derive(Default)]
    struct TestWindow {
        next_id: Cell<usize>,
        listeners: RefCell<Vec<TestListener>>,
    }

    impl TestWindow {
        fn listen(self: &Rc<Self>, event: PopupPositionEvent, callback: Rc<dyn Fn()>) -> Box<dyn FnOnce()> {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.listeners.borrow_mut().push(TestListener { id, event, callback });
            let window = Rc::clone(self);
            Box::new(move || window.listeners.borrow_mut().retain(|listener| listener.id != id))
        }

        fn dispatch(&self, event: PopupPositionEvent) {
            let callbacks: Vec<_> = self
                .listeners
                .borrow()
                .iter()
                .filter(|listener| listener.event == event)
                .map(|listener| Rc::clone(&listener.callback))
                .collect();
            for callback in callbacks {
                callback();
            }
        }
    }

    struct TestPopup {
        window: Rc<TestWindow>,
        anchor: Cell<Option<PopupAnchorRect>>,
        viewport: Cell<PopupSize>,
        fixed_origin: Cell<PopupPosition>,
        positions: RefCell<Vec<PopupPosition>>,
        closed: Cell<usize>,
    }

    impl TestPopup {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                window: Rc::new(TestWindow::default()),
                anchor: Cell::new(Some(PopupAnchorRect { top: 100.0, bottom: 140.0, left: 100.0, width: 40.0 })),
                viewport: Cell::new(VIEWPORT),
                fixed_origin: Cell::new(PopupPosition { top: 0.0, left: 0.0 }),
                positions: RefCell::new(Vec::new()),
                closed: Cell::new(0),
            })
        }

        fn open(self: &Rc<Self>) -> Box<dyn FnOnce()> {
            let popup = Rc::clone(self);
            let reposition = Rc::new(move || {
                let anchor = popup.anchor.get()?.in_fixed_coordinates(popup.fixed_origin.get());
                popup.positions.borrow_mut().push(popup_position(
                    anchor,
                    POPUP_SIZE,
                    popup.viewport.get(),
                    8.0,
                    PopupMenuPlacement::Adaptive,
                ));
                Some(())
            });
            let popup = Rc::clone(self);
            let on_close = Callback::from(move |()| popup.closed.set(popup.closed.get() + 1));
            let window = Rc::clone(&self.window);
            Box::new(track_popup_position(reposition, on_close, move |event, callback| window.listen(event, callback)))
        }
    }

    #[test]
    fn playlist_update_dropdown_opens_below_when_space_is_available() {
        let anchor = PopupAnchorRect { top: 100.0, bottom: 140.0, left: 100.0, width: 40.0 };
        assert_eq!(
            popup_position(anchor, POPUP_SIZE, VIEWPORT, 8.0, PopupMenuPlacement::Adaptive),
            PopupPosition { top: 148.0, left: 100.0 }
        );

        let exact_fit = PopupAnchorRect { top: 592.0, bottom: 632.0, left: 100.0, width: 40.0 };
        assert_eq!(
            popup_position(exact_fit, POPUP_SIZE, VIEWPORT, 8.0, PopupMenuPlacement::Adaptive),
            PopupPosition { top: 640.0, left: 100.0 }
        );
    }

    #[test]
    fn playlist_update_dropdown_flips_above_when_space_below_is_insufficient() {
        let anchor = PopupAnchorRect { top: 680.0, bottom: 720.0, left: 100.0, width: 40.0 };
        assert_eq!(
            popup_position(anchor, POPUP_SIZE, VIEWPORT, 8.0, PopupMenuPlacement::Adaptive),
            PopupPosition { top: 552.0, left: 100.0 }
        );
    }

    #[test]
    fn playlist_update_dropdown_bottom_end_remains_attached_near_viewport_edges() {
        let anchor = PopupAnchorRect { top: 680.0, bottom: 720.0, left: 600.0, width: 240.0 };
        let popup = PopupSize { width: 160.0, height: 120.0 };
        let position = popup_position(anchor, popup, VIEWPORT, 8.0, PopupMenuPlacement::BottomEnd);

        assert_eq!(position, PopupPosition { top: 728.0, left: 680.0 });
        assert_eq!(position.top, anchor.bottom + 8.0, "menu top must stay below the field");
        assert_eq!(position.left + popup.width, anchor.left + anchor.width, "right edges must remain attached");
        assert!(position.top + popup.height > VIEWPORT.height, "bottom anchoring must not flip or clamp upward");

        let scrolled_anchor = PopupAnchorRect { top: -180.0, bottom: -140.0, ..anchor };
        assert_eq!(
            popup_position(scrolled_anchor, popup, VIEWPORT, 8.0, PopupMenuPlacement::BottomEnd),
            PopupPosition { top: -132.0, left: 680.0 },
            "the menu must follow an off-screen field instead of sticking over the page header"
        );
    }

    #[test]
    fn playlist_update_dropdown_clamps_both_horizontal_viewport_edges() {
        for (anchor_left, expected_left) in [(-30.0, 8.0), (0.0, 8.0), (856.0, 856.0), (1000.0, 856.0)] {
            let anchor = PopupAnchorRect { top: 100.0, bottom: 140.0, left: anchor_left, width: 40.0 };
            assert_eq!(
                popup_position(anchor, POPUP_SIZE, VIEWPORT, 8.0, PopupMenuPlacement::Adaptive),
                PopupPosition { top: 148.0, left: expected_left }
            );
        }
    }

    #[test]
    fn playlist_update_dropdown_clamps_vertically_when_neither_side_fits() {
        let viewport = PopupSize { width: 320.0, height: 160.0 };
        let anchor = PopupAnchorRect { top: 60.0, bottom: 100.0, left: 100.0, width: 40.0 };
        assert_eq!(
            popup_position(anchor, POPUP_SIZE, viewport, 8.0, PopupMenuPlacement::Adaptive),
            PopupPosition { top: 32.0, left: 100.0 }
        );
        let above_viewport = PopupAnchorRect { top: -80.0, bottom: -40.0, left: 100.0, width: 40.0 };
        assert_eq!(
            popup_position(above_viewport, POPUP_SIZE, viewport, 8.0, PopupMenuPlacement::Adaptive),
            PopupPosition { top: 8.0, left: 100.0 }
        );
    }

    #[test]
    fn playlist_update_dropdown_scroll_repositions_from_the_new_anchor_rectangle() {
        let popup = TestPopup::new();
        let cleanup = popup.open();
        assert!(PopupPositionEvent::Scroll.capture(), "nested scroll events must be captured");
        assert_eq!(PopupPositionEvent::Scroll.name(), "scroll");
        popup.anchor.set(Some(PopupAnchorRect { top: 20.0, bottom: 60.0, left: 220.0, width: 40.0 }));
        popup.window.dispatch(PopupPositionEvent::Scroll);
        assert_eq!(
            *popup.positions.borrow(),
            [PopupPosition { top: 148.0, left: 100.0 }, PopupPosition { top: 68.0, left: 220.0 }]
        );
        cleanup();
    }

    #[test]
    fn playlist_update_dropdown_resize_repositions_and_rechecks_flip_and_clamping() {
        let popup = TestPopup::new();
        popup.anchor.set(Some(PopupAnchorRect { top: 180.0, bottom: 220.0, left: 220.0, width: 40.0 }));
        let cleanup = popup.open();
        assert_eq!(PopupPositionEvent::Resize.name(), "resize");
        popup.viewport.set(PopupSize { width: 240.0, height: 260.0 });
        popup.window.dispatch(PopupPositionEvent::Resize);
        assert_eq!(
            *popup.positions.borrow(),
            [PopupPosition { top: 228.0, left: 220.0 }, PopupPosition { top: 52.0, left: 72.0 }]
        );
        cleanup();
    }

    #[test]
    fn playlist_update_dropdown_cleanup_removes_listeners_and_invalidates_captured_callbacks() {
        let popup = TestPopup::new();
        let cleanup = popup.open();
        let pending_callbacks: Vec<_> =
            popup.window.listeners.borrow().iter().map(|listener| Rc::clone(&listener.callback)).collect();
        assert_eq!(popup.window.listeners.borrow().len(), 4);

        // This is the cleanup returned by the Yew effect for both close and unmount.
        cleanup();
        assert!(popup.window.listeners.borrow().is_empty());
        popup.window.dispatch(PopupPositionEvent::Scroll);
        popup.window.dispatch(PopupPositionEvent::Resize);
        popup.window.dispatch(PopupPositionEvent::VisualViewportScroll);
        popup.window.dispatch(PopupPositionEvent::VisualViewportResize);
        for callback in pending_callbacks {
            callback();
        }
        assert_eq!(popup.positions.borrow().len(), 1);
        assert_eq!(popup.closed.get(), 0);
    }

    #[test]
    fn playlist_update_dropdown_repeated_open_close_does_not_accumulate_handlers() {
        let popup = TestPopup::new();
        for cycle in 1..=20 {
            let cleanup = popup.open();
            assert_eq!(popup.window.listeners.borrow().len(), 4);
            popup.window.dispatch(PopupPositionEvent::Scroll);
            popup.window.dispatch(PopupPositionEvent::Resize);
            popup.window.dispatch(PopupPositionEvent::VisualViewportScroll);
            popup.window.dispatch(PopupPositionEvent::VisualViewportResize);
            assert_eq!(popup.positions.borrow().len(), cycle * 5);
            cleanup();
            assert!(popup.window.listeners.borrow().is_empty());
        }
        assert_eq!(popup.closed.get(), 0);
    }

    #[test]
    fn playlist_update_dropdown_missing_initial_anchor_closes_without_installing_listeners() {
        let popup = TestPopup::new();
        popup.anchor.set(None);
        let cleanup = popup.open();
        assert_eq!(popup.closed.get(), 1);
        assert!(popup.positions.borrow().is_empty());
        assert!(popup.window.listeners.borrow().is_empty());
        cleanup();
    }

    #[test]
    fn playlist_update_dropdown_invalidated_anchor_closes_once_without_reusing_coordinates() {
        let popup = TestPopup::new();
        let cleanup = popup.open();
        popup.anchor.set(None);
        popup.window.dispatch(PopupPositionEvent::Scroll);
        popup.window.dispatch(PopupPositionEvent::Resize);
        assert_eq!(popup.closed.get(), 1);
        assert_eq!(popup.positions.borrow().len(), 1);
        cleanup();
        assert!(popup.window.listeners.borrow().is_empty());
    }

    #[test]
    fn playlist_update_dropdown_matches_field_width_and_both_edges_on_mobile_and_desktop() {
        for viewport_width in [320.0, 390.0, 780.0, 781.0, 1024.0, 1499.0, 1920.0] {
            let viewport = PopupSize { width: viewport_width, height: 900.0 };
            let field_width = if viewport_width > 780.0 { 270.0 } else { viewport_width - 42.0 };
            let field = PopupAnchorRect {
                top: 99.0,
                bottom: 139.0,
                left: viewport_width - 21.0 - field_width,
                width: field_width,
            };
            let trigger = PopupAnchorRect { top: 100.0, bottom: 138.0, left: viewport_width - 69.0, width: 40.0 };
            let anchor = resolve_popup_anchor(PopupMenuWidth::MatchAnchor, trigger, Some(field)).unwrap();

            assert_eq!(anchor, field);
            let menu = PopupSize { width: anchor.width, height: 160.0 };
            let position = popup_position(anchor, menu, viewport, 8.0, PopupMenuPlacement::Adaptive);
            assert_eq!(position, PopupPosition { top: 147.0, left: field.left });
            assert_eq!(position.left + menu.width, field.left + field.width, "right edges must align");
        }
    }

    #[test]
    fn playlist_update_dropdown_default_keeps_the_trigger_anchor() {
        let trigger = PopupAnchorRect { top: 100.0, bottom: 140.0, left: 310.0, width: 40.0 };
        let field = PopupAnchorRect { top: 99.0, bottom: 141.0, left: 21.0, width: 348.0 };
        assert_eq!(PopupMenuWidth::default(), PopupMenuWidth::Content);
        assert_eq!(PopupMenuPlacement::default(), PopupMenuPlacement::Adaptive);
        assert_eq!(resolve_popup_anchor(PopupMenuWidth::Content, trigger, Some(field)), Some(trigger));
        assert_eq!(resolve_popup_anchor(PopupMenuWidth::Content, trigger, None), Some(trigger));
    }

    #[test]
    fn playlist_update_dropdown_missing_field_does_not_use_stale_trigger_geometry() {
        let trigger = PopupAnchorRect { top: 100.0, bottom: 140.0, left: 310.0, width: 40.0 };
        assert_eq!(resolve_popup_anchor(PopupMenuWidth::MatchAnchor, trigger, None), None);
    }

    #[test]
    fn playlist_update_dropdown_field_width_styles_apply_without_a_mobile_breakpoint() {
        let styles = include_str!("../../../scss/app/components/_popup_menu.scss");
        let field_rule = styles.split_once("&--match-anchor {").unwrap().1.split('}').next().unwrap();
        for declaration in ["width: var(--popup-anchor-width);", "max-width: calc(100vw - 1rem);"] {
            assert!(field_rule.contains(declaration), "missing field width declaration: {declaration}");
        }
        assert!(!styles.contains("@media"), "the same opt-in width must apply on desktop and mobile");
        assert!(styles.contains("box-sizing: border-box;"), "borders and padding must not widen the menu");
    }

    #[test]
    fn playlist_update_dropdown_visual_viewport_zoom_and_pan_keep_the_anchor_in_fixed_coordinates() {
        // Chrome uses layout-relative client rectangles; Safari can use visual-relative ones.
        for visual_relative in [false, true] {
            let popup = TestPopup::new();
            let cleanup = popup.open();
            let origin = if visual_relative {
                PopupPosition { top: -80.0, left: -50.0 }
            } else {
                PopupPosition { top: 0.0, left: 0.0 }
            };
            popup.fixed_origin.set(origin);
            popup.anchor.set(Some(PopupAnchorRect {
                top: 150.0 + origin.top,
                bottom: 190.0 + origin.top,
                left: 75.0 + origin.left,
                width: 160.0,
            }));
            popup.window.dispatch(PopupPositionEvent::VisualViewportResize);
            assert_eq!(popup.positions.borrow().last(), Some(&PopupPosition { top: 198.0, left: 75.0 }));

            // Pan only: no window scroll/resize, and the anchor's layout position is unchanged.
            if visual_relative {
                popup.fixed_origin.set(PopupPosition { top: -120.0, left: -90.0 });
                popup.anchor.set(Some(PopupAnchorRect { top: 30.0, bottom: 70.0, left: -15.0, width: 160.0 }));
            }
            popup.window.dispatch(PopupPositionEvent::VisualViewportScroll);
            assert_eq!(popup.positions.borrow().last(), Some(&PopupPosition { top: 198.0, left: 75.0 }));
            assert_eq!(popup.positions.borrow().len(), 3);
            assert_eq!(popup.closed.get(), 0);
            cleanup();
        }
    }

    #[test]
    fn playlist_update_dropdown_zoom_does_not_reflow_or_flip_the_full_width_field_menu() {
        let layout = PopupSize { width: 390.0, height: 844.0 };
        let field = PopupAnchorRect { top: 420.0, bottom: 458.0, left: 21.0, width: 348.0 };
        let menu = PopupSize { width: field.width, height: 160.0 };
        for origin in [
            PopupPosition { top: 0.0, left: 0.0 },
            PopupPosition { top: -200.0, left: -50.0 },
            PopupPosition { top: -300.0, left: -150.0 },
        ] {
            let client_rect = PopupAnchorRect {
                top: field.top + origin.top,
                bottom: field.bottom + origin.top,
                left: field.left + origin.left,
                ..field
            };
            let anchor = client_rect.in_fixed_coordinates(origin);
            assert_eq!(anchor, field, "panning must retain field width and layout position");
            assert_eq!(
                popup_position(anchor, menu, layout, 8.0, PopupMenuPlacement::Adaptive),
                PopupPosition { top: 466.0, left: 21.0 }
            );
        }
        let styles = include_str!("../../../scss/app/components/_popup_menu.scss");
        assert!(!styles.contains("--popup-viewport-"), "zoom viewport must not constrain menu dimensions");
        assert!(styles.contains("max-height: min(70vh, calc(100vh - 1rem));"));
    }

    #[test]
    fn playlist_update_dropdown_pinch_zoom_keeps_the_full_field_anchor_on_desktop() {
        let trigger = PopupAnchorRect { top: 100.0, bottom: 140.0, left: 800.0, width: 40.0 };
        let field = PopupAnchorRect { top: 99.0, bottom: 141.0, left: 600.0, width: 240.0 };
        let layout = PopupSize { width: 1024.0, height: 768.0 };
        let anchor = resolve_popup_anchor(PopupMenuWidth::MatchAnchor, trigger, Some(field)).unwrap();
        assert_eq!(anchor, field, "field matching must not depend on the pinch-zoom viewport");
        assert_eq!(
            popup_position(
                anchor,
                PopupSize { width: anchor.width, height: POPUP_SIZE.height },
                layout,
                8.0,
                PopupMenuPlacement::Adaptive,
            ),
            PopupPosition { top: 149.0, left: 600.0 }
        );
    }

    #[test]
    fn playlist_update_dropdown_visual_viewport_events_have_their_own_targets_and_cleanup() {
        for (event, name) in
            [(PopupPositionEvent::VisualViewportScroll, "scroll"), (PopupPositionEvent::VisualViewportResize, "resize")]
        {
            assert_eq!(event.name(), name);
            assert!(!event.capture());
            let popup = TestPopup::new();
            let cleanup = popup.open();
            assert!(popup.window.listeners.borrow().iter().any(|listener| listener.event == event));
            popup.anchor.set(None);
            popup.window.dispatch(event);
            popup.window.dispatch(event);
            assert_eq!(popup.closed.get(), 1);
            cleanup();
            assert!(popup.window.listeners.borrow().is_empty());
        }
    }

    #[test]
    fn playlist_update_view_policy_menu_keyboard_navigation_wraps_and_handles_boundaries() {
        assert_eq!(next_menu_item_index("ArrowDown", None, 3), Some(0));
        assert_eq!(next_menu_item_index("ArrowDown", Some(2), 3), Some(0));
        assert_eq!(next_menu_item_index("ArrowUp", None, 3), Some(2));
        assert_eq!(next_menu_item_index("ArrowUp", Some(0), 3), Some(2));
        assert_eq!(next_menu_item_index("Home", Some(2), 3), Some(0));
        assert_eq!(next_menu_item_index("End", Some(0), 3), Some(2));
        assert_eq!(next_menu_item_index("Tab", Some(0), 3), None);
        assert_eq!(next_menu_item_index("ArrowDown", None, 0), None);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    // Optional browser-driver hook for real pinch/pan gestures. Ordinary browser test runs
    // still exercise the DOM listeners below, without requiring a particular driver.
    #[wasm_bindgen::prelude::wasm_bindgen(
        inline_js = "export async function drivePopupZoom() { await window.drivePopupZoom?.(); }"
    )]
    extern "C" {
        #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = drivePopupZoom)]
        async fn drive_popup_zoom() -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
    }

    #[derive(Properties, PartialEq)]
    struct MobileFieldProps {
        on_close: Callback<()>,
    }

    #[component]
    fn MobileField(props: &MobileFieldProps) -> Html {
        let field_ref = use_node_ref();
        let trigger_ref = use_node_ref();
        let anchor = use_state(|| None::<web_sys::Element>);
        {
            let anchor = anchor.clone();
            let trigger_ref = trigger_ref.clone();
            use_effect_with((), move |()| anchor.set(trigger_ref.cast()));
        }
        html! {
            <>
                <div id="popup-test-field" ref={field_ref.clone()}
                    style="position:fixed;top:20px;left:21px;width:calc(100vw - 42px);height:38px">
                    <button ref={trigger_ref}>{"Force Update"}</button>
                </div>
                if let Some(anchor) = (*anchor).clone() {
                    <PopupMenu is_open={true} anchor_ref={Some(anchor)}
                        width={PopupMenuWidth::MatchAnchor} width_anchor_ref={Some(field_ref)}
                        on_close={props.on_close.clone()}>
                        <button>{"Update"}</button>
                        <button>{"Refresh"}</button>
                        <button>{"Force Update"}</button>
                    </PopupMenu>
                }
            </>
        }
    }

    #[wasm_bindgen_test(async)]
    async fn playlist_update_dropdown_browser_tracks_visual_viewport_events_and_cleans_up() {
        let window = window().unwrap();
        let document = window.document().unwrap();
        let root = document.create_element("div").unwrap();
        let meta = document.create_element("meta").unwrap();
        meta.set_attribute("name", "viewport").unwrap();
        meta.set_attribute("content", "width=device-width, initial-scale=1").unwrap();
        document.head().unwrap().append_child(&meta).unwrap();
        let style = document.create_element("style").unwrap();
        style.set_text_content(Some(
            ".tp__popup-menu {position:fixed;top:var(--popup-top);left:var(--popup-left);width:var(--popup-anchor-width);height:160px}",
        ));
        let body = document.body().unwrap();
        for element in [&root, &style] {
            body.append_child(element).unwrap();
        }
        let closed = Rc::new(Cell::new(0));
        let on_close = {
            let closed = Rc::clone(&closed);
            Callback::from(move |()| closed.set(closed.get() + 1))
        };
        let handle =
            yew::Renderer::<MobileField>::with_root_and_props(root.clone(), MobileFieldProps { on_close }).render();
        TimeoutFuture::new(0).await;
        TimeoutFuture::new(0).await;
        let anchor = document.get_element_by_id("popup-test-field").unwrap();
        let popup = document.query_selector(".tp__popup-menu").unwrap().unwrap();
        let visual = window.visual_viewport().unwrap();
        let second = popup.query_selector_all("button").unwrap().item(1).unwrap().dyn_into::<HtmlElement>().unwrap();
        second.focus().unwrap();
        for (event, top) in [("resize", 50), ("scroll", 70)] {
            anchor
                .set_attribute(
                    "style",
                    &format!("position:fixed;top:{top}px;left:21px;width:calc(100vw - 42px);height:38px"),
                )
                .unwrap();
            // No window event is dispatched: this exercises the actual VisualViewport listener.
            visual.dispatch_event(&Event::new(event).unwrap()).unwrap();
            let anchor_rect = anchor.get_bounding_client_rect();
            let popup_rect = popup.get_bounding_client_rect();
            assert!((popup_rect.top() - anchor_rect.bottom() - 8.0).abs() < 0.5);
            assert!((popup_rect.left() - anchor_rect.left()).abs() < 0.5);
            assert!((popup_rect.width() - anchor_rect.width()).abs() < 0.5);
            assert_eq!(document.active_element(), Some(second.clone().into()), "repositioning must preserve focus");
        }
        drive_popup_zoom().await.unwrap();
        handle.destroy();
        TimeoutFuture::new(0).await;
        anchor.remove();
        visual.dispatch_event(&Event::new("resize").unwrap()).unwrap();
        visual.dispatch_event(&Event::new("scroll").unwrap()).unwrap();
        assert_eq!(closed.get(), 0, "unmounted menus must not retain VisualViewport callbacks");
        assert!(document.query_selector(".tp__popup-menu").unwrap().is_none());
        root.remove();
        style.remove();
        meta.remove();
    }
}
