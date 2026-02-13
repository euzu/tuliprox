use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
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

#[function_component]
pub fn PopupMenu(props: &PopupMenuProps) -> Html {
    let popup_ref = use_node_ref();

    // Calculate popup position relative to anchor and keep inside viewport
    let style = {
        let is_open = props.is_open;
        let anchor_ref = props.anchor_ref.clone();
        let popup_ref = popup_ref.clone();
        use_memo(
            (is_open, anchor_ref.clone()),
            move |(is_open, anchor_ref)| {
                if !*is_open || anchor_ref.is_none() {
                    return "hidden".to_string();
                }
                let anchor_ref = anchor_ref.as_ref().unwrap().clone();

                let rect = anchor_ref.get_bounding_client_rect();
                let window = window().expect("no global window");
                let inner_width = window.inner_width().unwrap().as_f64().unwrap();
                let inner_height = window.inner_height().unwrap().as_f64().unwrap();

                // Basic positioning below the anchor element
                let mut top = rect.bottom();
                let mut left = rect.left();

                // Clamp popup within viewport width (assuming popup width ~200px)
                if left + 200.0 > inner_width {
                    left = inner_width - 200.0;
                }
                if top + 150.0 > inner_height {
                    // show above if no space below (assuming popup height ~150px)
                    top = rect.top() - 150.0;
                }

                if let Some(popup) = popup_ref.cast::<HtmlElement>() {
                    let _ = popup
                        .style()
                        .set_property("--popup-top", &format!("{top}px"));
                    let _ = popup
                        .style()
                        .set_property("--popup-left", &format!("{left}px"));
                }
                "".to_owned()
            },
        )
    };

    // Close popup when clicking outside of it
    {
        let popup_ref = popup_ref.clone();
        let on_close = props.on_close.clone();
        use_effect_with(props.is_open, move |is_open| {
            let handler = if *is_open {
                let handler = Closure::wrap(Box::new(move |event: MouseEvent| {
                    if let Some(popup) = popup_ref.cast::<HtmlElement>() {
                        if let Some(target) = event
                            .target()
                            .and_then(|t| t.dyn_into::<HtmlElement>().ok())
                        {
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
                        .remove_event_listener_with_callback(
                            "mousedown",
                            handler.as_ref().unchecked_ref(),
                        )
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
