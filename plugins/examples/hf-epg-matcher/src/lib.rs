//! HF EPG Matcher: a `match-channel` plugin.
//!
//! The host calls `match-channels` only for live channels the built-in matcher
//! left without a guide id, and only with a shortlist of guide channels that
//! kept programmes (`candidates`). For each request this plugin asks a
//! HuggingFace sentence-similarity model how close the channel name is to every
//! candidate name, and answers the best candidate at or above the configured
//! threshold.
//!
//! - The decision for a channel name is cached in the plugin KV under
//!   `match:<model>:<fnv1a of the normalized name>`, so the model is asked once
//!   per unknown name (and again only when the shortlist for that name changed).
//! - The API being down never breaks the run: the uncached requests are answered
//!   `None` and nothing is cached for them.
//! - Output is a pure function of (requests, settings, KV, API answers): no
//!   randomness, no wall clock, ties go to the earlier candidate.

mod engine;

use engine::Options;
use tuliprox_plugin_sdk::prelude::*;

fn default_model_id() -> String { engine::DEFAULT_MODEL_ID.to_string() }
const fn default_threshold() -> f64 { engine::DEFAULT_THRESHOLD }
const fn default_batch_size() -> u32 { engine::DEFAULT_BATCH_SIZE }
fn default_api_base_url() -> String { engine::DEFAULT_API_BASE_URL.to_string() }

#[tuliprox_plugin]
mod plugin {
    use super::*;

    #[tuliprox_plugin_settings]
    struct Settings {
        #[serde(default = "default_model_id")]
        model_id: String,
        #[serde(default)]
        hf_token: Secret<String>,
        #[serde(default = "default_threshold")]
        threshold: f64,
        #[serde(default = "default_batch_size")]
        batch_size: u32,
        #[serde(default = "default_api_base_url")]
        api_base_url: String,
    }

    pub struct Matcher;

    impl EpgMatchHooks for Matcher {
        fn match_channels(requests: Vec<MatchRequest>) -> Result<Vec<Option<String>>, String> {
            let settings = Settings::current()?;
            let options = Options {
                model_id: settings.model_id.clone(),
                token: settings.hf_token.expose().clone(),
                threshold: settings.threshold,
                batch_size: settings.batch_size,
                api_base_url: settings.api_base_url.clone(),
            };
            Ok(engine::match_requests(&options, &requests))
        }
    }
}

#[cfg(test)]
mod tests;
