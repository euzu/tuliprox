//! Sports Enricher: an example tuliprox plugin that touches every part of the plugin system.
//!
//! | Hook | Module | What it does |
//! | --- | --- | --- |
//! | `playlist.after-filter` | [`playlist`] | regroups sport channels (`⚽ Football`), logos via the image cache |
//! | `playlist.after-sort` | [`playlist`] | orders channels by league, then match time |
//! | `playlist.enrich-metadata` | [`metadata`] | sports documentaries get genre, plot and description |
//! | `epg.enrich-programmes` | [`guide`] | `Home vs Away` programmes from fixtures |
//! | `events.on-event` | [`activity`] | remembers the last `playlist.update` |
//! | `metrics.collect` | [`activity`] | fixtures cached, API calls today, channels enriched |
//! | `tasks.run` | [`api`] | `refresh-fixtures` |
//! | `rpc.call` | [`api`] | `get-fixtures`, used by the UI frames and MCP |
//!
//! Fixtures come from a sports API over `wasi:http` ([`fixtures`]) and are cached in the plugin KV
//! store with TTL keys. Every hook is a pure function of its input, the settings, the KV store and
//! the host clock, and fails open: an unreachable API never fails a playlist.

pub mod activity;
pub mod api;
pub mod fixtures;
pub mod guide;
pub mod metadata;
pub mod playlist;
pub mod settings;
pub mod sports;
#[cfg(test)]
mod testing;
pub mod timefmt;

use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin]
mod plugin {
    use super::*;

    /// Settings, from `config-schema.json`. The default `lifecycle` parses them at `init`.
    #[tuliprox_plugin_settings]
    pub struct Settings {
        /// Bearer token of the sports API. A write-only secret: never logged, never returned.
        #[serde(default)]
        pub api_key: Option<Secret<String>>,
        #[serde(default = "crate::settings::default_api_base_url")]
        pub api_base_url: String,
        /// League ids to track; empty means the defaults.
        #[serde(default)]
        pub leagues: Vec<String>,
        /// Prefix group names with a sport emoji.
        #[serde(default = "crate::settings::default_group_emoji")]
        pub group_emoji: bool,
    }

    pub struct SportsEnricher;

    impl PlaylistHooks for SportsEnricher {
        fn on_stage(stage: Stage, _target: &str, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
            // The manifest lists only playlist.after-filter.
            if !matches!(stage, Stage::AfterFilter) {
                return Ok(Vec::new());
            }
            let settings = Settings::current()?;
            Ok(playlist::after_filter(&settings, channels))
        }
    }

    impl SortHooks for SportsEnricher {
        fn after_sort(_target: &str, channels: Vec<Channel>) -> Result<Vec<u32>, String> {
            let settings = Settings::current()?;
            Ok(playlist::after_sort(&settings, &channels))
        }
    }

    impl MetadataHooks for SportsEnricher {
        fn enrich_metadata(item: Channel, metadata_json: &str) -> Result<Option<String>, String> {
            let settings = Settings::current()?;
            metadata::enrich(&settings, &item, metadata_json)
        }
    }

    impl EpgEnrichHooks for SportsEnricher {
        fn enrich_programmes(channel_id: &str, programmes: Vec<EpgProgramme>) -> Result<Vec<EpgProgramme>, String> {
            let settings = Settings::current()?;
            Ok(guide::enrich(&settings, channel_id, programmes))
        }
    }

    impl EventHooks for SportsEnricher {
        fn on_event(ev: Event) -> Result<(), String> { activity::on_event(&ev) }
    }

    impl MetricsHooks for SportsEnricher {
        fn collect_metrics() -> Result<Vec<Metric>, String> {
            let settings = Settings::current()?;
            Ok(activity::collect(&settings))
        }
    }

    impl Tasks for SportsEnricher {
        fn run(ctx: TaskContext) -> Result<String, String> { api::run_task(&ctx) }
    }

    impl Rpc for SportsEnricher {
        fn call(method: &str, payload: &str) -> Result<String, String> { api::call(method, payload) }
    }
}

pub use plugin::Settings;
