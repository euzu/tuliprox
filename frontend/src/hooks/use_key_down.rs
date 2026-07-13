use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, KeyboardEvent};
use yew::prelude::*;

/// Returns `true` when the event originates from a control where typing should
/// take precedence over global shortcuts (text inputs, textareas, selects or
/// content-editable elements). Global handlers that would otherwise swallow a
/// key (e.g. Delete) should bail out early when this is `true`.
pub fn is_text_input_focused(event: &KeyboardEvent) -> bool {
    let Some(element) = event.target().and_then(|t| t.dyn_into::<HtmlElement>().ok()) else {
        return false;
    };
    let tag = element.tag_name().to_lowercase();
    matches!(tag.as_str(), "input" | "textarea" | "select") || element.is_content_editable()
}

/// Registers a window-level `keydown` listener for the lifetime of the component
/// and re-registers it whenever `deps` change. The listener is removed on
/// cleanup, so callers never have to manage `add`/`remove_event_listener`
/// boilerplate themselves.
#[hook]
pub fn use_key_down<D, F>(deps: D, handler: F)
where
    D: PartialEq + 'static,
    F: Fn(&KeyboardEvent) + 'static,
{
    use_effect_with(deps, move |_| {
        let closure = Closure::<dyn FnMut(KeyboardEvent)>::wrap(Box::new(move |event: KeyboardEvent| {
            handler(&event);
        }));

        if let Some(win) = window() {
            let _ = win.add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref());
        }

        move || {
            if let Some(win) = window() {
                let _ = win.remove_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref());
            }
        }
    });
}
