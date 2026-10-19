use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::BeforeUnloadEvent;
use yew::prelude::*;

/// Asks the browser for confirmation before it unloads the page (refresh, close, navigation) while `dirty`.
#[hook]
pub fn use_unsaved_changes_guard(dirty: bool) {
    use_effect_with(dirty, move |&dirty| {
        let listener = dirty
            .then(|| {
                let window = web_sys::window()?;
                let closure =
                    Closure::<dyn FnMut(BeforeUnloadEvent)>::wrap(Box::new(move |event: BeforeUnloadEvent| {
                        event.prevent_default();
                        event.set_return_value("");
                    }));
                window.add_event_listener_with_callback("beforeunload", closure.as_ref().unchecked_ref()).ok()?;
                Some((window, closure))
            })
            .flatten();
        move || {
            if let Some((window, closure)) = listener {
                let _ = window.remove_event_listener_with_callback("beforeunload", closure.as_ref().unchecked_ref());
            }
        }
    });
}
