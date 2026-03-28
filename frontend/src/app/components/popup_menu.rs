use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, MouseEvent};
use yew::prelude::*;

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
        });
    }

    // Close popup when clicking outside of it
    {
        let popup_ref = popup_ref.clone();
        let on_close = props.on_close.clone();
        use_effect_with(props.is_open, move |is_open| {
            let handler = if *is_open {
                let handler = Closure::wrap(Box::new(move |event: MouseEvent| {
                    if let Some(popup) = popup_ref.cast::<HtmlElement>() {
                        if let Some(target) = event.target().and_then(|t| t.dyn_into::<HtmlElement>().ok()) {
                            if !popup.contains(Some(&target)) {
                                on_close.emit(());
                            }
                        }
                    }
                }) as Box<dyn FnMut(_)>);

                window()
                    .unwrap()
                    .add_event_listener_with_callback("mousedown", handler.as_ref().unchecked_ref())
                    .unwrap();
                Some(handler)
            } else {
                None
            };

            // Cleanup-Funktion
            move || {
                if let Some(handler) = handler {
                    window()
                        .unwrap()
                        .remove_event_listener_with_callback("mousedown", handler.as_ref().unchecked_ref())
                        .unwrap();
                }
            }
        });
    }

    html! {
        <div class={classes!("tp__popup-menu", (*style).clone())} ref={popup_ref}>
            <ul>
                { for props.children.iter().map(|child| html! { <li>{child.clone()}</li> }) }
            </ul>
        </div>
    }
}
