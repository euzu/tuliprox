use crate::hooks::use_key_down;
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, KeyboardEvent, MouseEvent};
use yew::{create_portal, prelude::*};

#[derive(Properties, PartialEq, Clone)]
pub struct PopupMenuProps {
    pub is_open: bool,
    pub anchor_ref: Option<web_sys::Element>,
    #[prop_or_default]
    pub on_close: Callback<()>,
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
            "".to_owned()
        })
    };

    {
        let popup_ref = popup_ref.clone();
        let anchor_ref = props.anchor_ref.clone();
        use_effect_with((props.is_open, anchor_ref, popup_ref.clone()), move |(is_open, anchor_ref, popup_ref)| {
            if !*is_open {
                return;
            }
            let Some(anchor) = anchor_ref.as_ref() else {
                return;
            };
            let Some(popup) = popup_ref.cast::<HtmlElement>() else {
                return;
            };
            let Some(window) = window() else {
                return;
            };

            let rect = anchor.get_bounding_client_rect();
            let inner_width = window.inner_width().ok().and_then(|w| w.as_f64()).unwrap_or_default();
            let inner_height = window.inner_height().ok().and_then(|h| h.as_f64()).unwrap_or_default();
            let popup_width = f64::from(popup.offset_width());
            let popup_height = f64::from(popup.offset_height());
            let gutter = 8.0;

            let mut top = rect.bottom() + gutter;
            let mut left = rect.left();

            if left + popup_width > inner_width - gutter {
                left = inner_width - popup_width - gutter;
            }
            if top + popup_height > inner_height - gutter {
                let top_above = rect.top() - popup_height - gutter;
                top = if top_above >= gutter { top_above } else { inner_height - popup_height - gutter };
            }
            left = left.max(gutter);
            top = top.max(gutter);

            let _ = popup.style().set_property("--popup-top", &format!("{top}px"));
            let _ = popup.style().set_property("--popup-left", &format!("{left}px"));

            // Move focus into the menu so keyboard users can navigate immediately
            if let Ok(Some(first)) = popup.query_selector("button") {
                if let Ok(button) = first.dyn_into::<HtmlElement>() {
                    let _ = button.focus();
                }
            }
        });
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
            let next = match key.as_str() {
                "ArrowDown" => Some(current.map_or(0, |c| (c + 1) % count)),
                "ArrowUp" => Some(current.map_or(count - 1, |c| (c + count - 1) % count)),
                "Home" => Some(0),
                "End" => Some(count - 1),
                _ => None,
            };
            if let Some(idx) = next {
                event.prevent_default();
                if let Some(item) = items.item(idx).and_then(|node| node.dyn_into::<HtmlElement>().ok()) {
                    let _ = item.focus();
                }
            }
        });
    }

    let popup = html! {
        <div class={classes!("tp__popup-menu", (*style).clone())} ref={popup_ref}>
            <ul role="menu">
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
