use wasm_bindgen::JsCast;
use web_sys::{window, Element, HtmlElement, KeyboardEvent, MouseEvent};
use yew::prelude::*;

const FOCUSABLE_SELECTOR: &str = "a[href], button:not([disabled]), input:not([disabled]), \
select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex=\"-1\"])";

fn focusable_elements(container: &Element) -> Vec<HtmlElement> {
    let mut result = Vec::new();
    if let Ok(nodes) = container.query_selector_all(FOCUSABLE_SELECTOR) {
        for i in 0..nodes.length() {
            if let Some(element) = nodes.item(i).and_then(|node| node.dyn_into::<HtmlElement>().ok()) {
                // `offset_parent` is `None` for elements that are not rendered
                // (e.g. `display: none`), which should not receive focus.
                if element.offset_parent().is_some() {
                    result.push(element);
                }
            }
        }
    }
    result
}

#[derive(Properties, PartialEq)]
pub struct CustomDialogProps {
    pub children: Children,
    pub class: Option<String>,
    #[prop_or(true)]
    pub open: bool,
    #[prop_or(true)]
    pub modal: bool,
    #[prop_or(false)]
    pub close_on_backdrop_click: bool,
    pub on_close: Option<Callback<()>>,
    #[prop_or_default]
    pub aria_label: Option<String>,
}

#[component]
pub fn CustomDialog(props: &CustomDialogProps) -> Html {
    let is_open = use_state(|| props.open);
    let dialog_ref = use_node_ref();
    let previously_focused = use_mut_ref(|| None::<HtmlElement>);

    // Update state when props change
    {
        let is_open = is_open.clone();
        let previously_focused = previously_focused.clone();
        use_effect_with(props.open, move |&open| {
            if open {
                *previously_focused.borrow_mut() = window()
                    .and_then(|w| w.document())
                    .and_then(|document| document.active_element())
                    .and_then(|el| el.dyn_into::<HtmlElement>().ok());
            }
            is_open.set(open);
            || ()
        });
    }

    {
        let dialog_ref = dialog_ref.clone();
        let previously_focused = previously_focused.clone();
        use_effect_with(*is_open, move |open| {
            if *open {
                if let Some(document) = window().and_then(|w| w.document()) {
                    if let Some(container) = dialog_ref.cast::<HtmlElement>() {
                        let focus_inside =
                            document.active_element().is_some_and(|active| container.contains(Some(active.as_ref())));
                        if !focus_inside {
                            let _ = container.focus();
                        }
                    }
                }
            }

            let previously_focused = previously_focused.clone();
            move || {
                if let Some(previous) = previously_focused.borrow_mut().take() {
                    let _ = previous.focus();
                }
            }
        });
    }

    // Handle backdrop click
    let on_backdrop_click = {
        let on_close = props.on_close.clone();
        let close_on_backdrop = props.close_on_backdrop_click;

        Callback::from(move |_e: MouseEvent| {
            if close_on_backdrop {
                if let Some(on_close) = &on_close {
                    on_close.emit(());
                }
            }
        })
    };

    // Close on Escape
    let on_key_down = {
        let dialog_ref = dialog_ref.clone();
        let on_close = props.on_close.clone();
        let dismissable = props.close_on_backdrop_click;

        Callback::from(move |event: KeyboardEvent| match event.key().as_str() {
            "Escape" => {
                event.stop_propagation();
                if dismissable {
                    if let Some(on_close) = &on_close {
                        event.prevent_default();
                        on_close.emit(());
                    }
                }
            }
            "Tab" => {
                event.stop_propagation();
                let Some(container) = dialog_ref.cast::<Element>() else {
                    return;
                };
                let focusables = focusable_elements(&container);
                let Some(first) = focusables.first() else {
                    event.prevent_default();
                    return;
                };
                let last = focusables.last().unwrap_or(first);
                let active = window().and_then(|w| w.document()).and_then(|d| d.active_element());

                if event.shift_key() {
                    if active.as_ref().is_some_and(|a| a.is_same_node(Some(first.as_ref()))) {
                        event.prevent_default();
                        let _ = last.focus();
                    }
                } else if active.as_ref().is_some_and(|a| a.is_same_node(Some(last.as_ref()))) {
                    event.prevent_default();
                    let _ = first.focus();
                }
            }
            _ => {}
        })
    };

    // Only render if open
    if !*is_open {
        return html! {};
    }

    html! {
        <div class={classes!("tp__custom-dialog-backdrop", if props.modal {"tp__custom-dialog-modal"} else {""})} onclick={on_backdrop_click}>
            <div
                ref={dialog_ref}
                class={classes!("tp__custom-dialog", props.class.as_ref().map_or_else(||"".to_owned(), |s|s.clone()))}
                role="dialog"
                aria-modal={props.modal.to_string()}
                aria-label={props.aria_label.clone()}
                tabindex="-1"
                onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}
                onkeydown={on_key_down}
            >
                { for props.children.iter() }
            </div>
        </div>
    }
}
