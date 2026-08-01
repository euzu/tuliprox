mod format;
mod storage;

use crate::i18n::YewI18n;
pub use format::*;
use shared::model::{PlaylistItemType, StreamInfo};
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
   ($cond:expr, $body:tt, $else:tt) => {
        if $cond {
            yew::html! $body
        } else {
            yew::html! $else
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

pub fn encoding_for_query(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_else(|| s.to_string())
}

pub fn join_non_empty_parts<'a>(parts: impl Iterator<Item = &'a str>, separator: &str) -> String {
    let mut result = String::new();
    for part in parts.filter(|part| !part.is_empty()) {
        if !result.is_empty() {
            result.push_str(separator);
        }
        result.push_str(part);
    }
    result
}

pub fn is_shared_hls_stream(stream: &StreamInfo) -> bool {
    stream.channel.shared
        && (stream.channel.item_type == PlaylistItemType::LiveHls
            || stream.channel.item_type == PlaylistItemType::Catchup)
}
