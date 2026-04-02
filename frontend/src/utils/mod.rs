mod format;
mod storage;

use crate::i18n::YewI18n;
pub use format::*;
pub use storage::*;
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::window;

#[macro_export]
macro_rules! html_if {
    ($cond:expr, $body:tt) => {
        if $cond {
            yew::html! $body
        } else {
            yew::Html::default()
        }
    };
}

pub use html_if;

pub fn set_timeout<F>(callback: F, millis: i32)
where
    F: FnOnce() + 'static,
{
    let cb = Closure::once_into_js(Box::new(callback) as Box<dyn FnOnce()>);
    window().unwrap().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), millis).unwrap();
}

pub fn t_safe(i18n: &YewI18n, key: &str) -> Option<String> {
    let result = i18n.t(key);

    if result.starts_with("Unable to find the key")
        || (result.starts_with("Translation key '") && result.ends_with("' not found."))
        || (result.starts_with("Key '") && result.contains("' not found for language '"))
    {
        None
    } else {
        Some(result)
    }
}
