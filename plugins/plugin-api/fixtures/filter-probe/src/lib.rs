//! Fixture: `eval-filter-batch` with one function, `name-contains(needle)`.
//!
//! The decision for a channel is true when its name contains the argument. An unknown
//! function name, or a missing argument, is an `err`, which the host counts as a failed run.

wit_bindgen::generate!({
    path: "../../wit",
    world: "filter-plugin",
    generate_all,
});

use exports::tuliprox::plugin::{filter_hooks, lifecycle};
use tuliprox::plugin::types::Channel;

struct FilterProbe;

impl lifecycle::Guest for FilterProbe {
    fn init(_settings: String) -> Result<(), String> { Ok(()) }
}

impl filter_hooks::Guest for FilterProbe {
    fn eval_filter_batch(name: String, channels: Vec<Channel>, args: Vec<String>) -> Result<Vec<bool>, String> {
        match name.as_str() {
            "name-contains" => {
                let needle = args.first().ok_or_else(|| "name-contains needs one argument".to_string())?;
                Ok(channels.iter().map(|channel| channel.name.contains(needle.as_str())).collect())
            }
            other => Err(format!("unknown function `{other}`")),
        }
    }
}

export!(FilterProbe);
