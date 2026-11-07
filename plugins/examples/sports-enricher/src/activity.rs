//! What the plugin remembers about the host's refreshes (`events.on-event`) and what it reports
//! to the dashboard (`metrics.collect`).
//!
//! There is no server-to-frame push in the bridge. The UI frames poll the `get-fixtures` rpc, which
//! returns [`LastUpdate`], and show a "Fixtures refreshed" toast when its `at` moves.

use crate::{fixtures, Settings};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuliprox_plugin_sdk::{events::PlaylistUpdate, prelude::*};

pub const KEY_LAST_UPDATE: &str = "last-update";
/// The plugin id as the host reports it in `PipelineStats.plugins`.
const PLUGIN_ID: &str = "sports-enricher";

/// One `playlist.update`, reduced to what the plugin and its frames show.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastUpdate {
    /// Unix seconds the event carried.
    pub at: i64,
    pub state: String,
    /// Channels the refresh produced, over all inputs.
    pub channels: u64,
    /// Channels this plugin was offered in `after-filter`.
    pub sport_channels: u64,
    /// Channels `after-filter` changed (regrouped or given a logo): "channels enriched".
    pub regrouped: u64,
    /// Programmes `enrich-programmes` added.
    pub programmes_added: u64,
    /// VOD records `enrich-metadata` amended.
    pub vod_enriched: u64,
}

fn number(value: &Value, key: &str) -> u64 { value.get(key).and_then(Value::as_u64).unwrap_or(0) }

/// Reduce the event. `stats` is the server's `[{inputs: [...], targets: [...]}]`; the plugin's own
/// hook counters are under `targets[].processing.plugins.hooks[]`.
#[must_use]
pub fn summarize(at: i64, update: &PlaylistUpdate) -> LastUpdate {
    let mut last = LastUpdate { at, state: format!("{:?}", update.state), ..LastUpdate::default() };
    for source in &update.stats {
        for input in source.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            last.channels += input.get("processed").map_or(0, |processed| number(processed, "channels"));
        }
        for target in source.get("targets").and_then(Value::as_array).into_iter().flatten() {
            let hooks = target.pointer("/processing/plugins/hooks").and_then(Value::as_array);
            for hook in hooks.into_iter().flatten() {
                if hook.get("plugin").and_then(Value::as_str) != Some(PLUGIN_ID) {
                    continue;
                }
                let modified = number(hook, "modified");
                match hook.get("hook").and_then(Value::as_str) {
                    Some("after-filter") => {
                        last.sport_channels += number(hook, "kept");
                        last.regrouped += modified;
                    }
                    Some("enrich-programmes") => last.programmes_added += modified,
                    Some("enrich-metadata") => last.vod_enriched += modified,
                    _ => {}
                }
            }
        }
    }
    last
}

/// The last recorded refresh.
#[must_use]
pub fn last_update() -> Option<LastUpdate> { kv::get_json::<LastUpdate>(KEY_LAST_UPDATE).ok().flatten() }

/// `events.on-event`: remember a `playlist.update`.
pub fn on_event(event: &Event) -> Result<(), String> {
    match event.typed()? {
        TypedEvent::PlaylistUpdate(update) => {
            let at = if event.timestamp > 0 { event.timestamp } else { clock::now_unix() };
            let last = summarize(at, &update);
            info!(
                "playlist refresh recorded: {} channels, {} regrouped, {} programmes added",
                last.channels, last.regrouped, last.programmes_added
            );
            kv::set_json(KEY_LAST_UPDATE, &last)?;
        }
        other => debug!("ignoring {other:?}"),
    }
    Ok(())
}

/// `metrics.collect`: fixtures cached, API calls today, channels enriched by the last refresh.
#[must_use]
pub fn collect(settings: &Settings) -> Vec<Metric> {
    let cached = fixtures::load(settings, false).fixture_count();
    let enriched = last_update().map_or(0, |last| last.regrouped);
    metrics! {
        "fixtures_cached", "metric.fixtures_cached" => cached;
        "api_calls_today", "metric.api_calls_today" => fixtures::calls_today(), unit = "calls";
        "channels_enriched", "metric.channels_enriched" => enriched;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::testing::{cache, fixture, league},
        testing::settings,
    };
    use tuliprox_plugin_sdk::mock;

    fn event(payload: Value, timestamp: i64) -> Event {
        Event { kind: "playlist.update".to_string(), payload: payload.to_string(), timestamp }
    }

    fn payload() -> Value {
        serde_json::json!({
            "state": "Success",
            "stats": [{
                "inputs": [
                    {"name": "a", "processed": {"groups": 2, "channels": 10}},
                    {"name": "b", "processed": {"groups": 1, "channels": 5}},
                ],
                "targets": [{
                    "target": "t", "success": true,
                    "processing": {"plugins": {"hooks": [
                        {"plugin": "sports-enricher", "hook": "after-filter", "kept": 7, "modified": 6},
                        {"plugin": "sports-enricher", "hook": "enrich-programmes", "kept": 9, "modified": 4},
                        {"plugin": "sports-enricher", "hook": "enrich-metadata", "kept": 1, "modified": 1},
                        {"plugin": "someone-else", "hook": "after-filter", "kept": 99, "modified": 99},
                    ]}}
                }]
            }]
        })
    }

    #[test]
    fn a_playlist_update_is_reduced_to_this_plugins_counters() {
        mock::reset();
        on_event(&event(payload(), 4_242)).unwrap();
        assert_eq!(
            last_update(),
            Some(LastUpdate {
                at: 4_242,
                state: "Success".to_string(),
                channels: 15,
                sport_channels: 7,
                regrouped: 6,
                programmes_added: 4,
                vod_enriched: 1,
            })
        );
    }

    #[test]
    fn a_bare_event_and_other_kinds_are_tolerated() {
        mock::reset();
        mock::set_now(77);
        on_event(&event(serde_json::json!({"state": "Failure", "error": "x"}), 0)).unwrap();
        let last = last_update().unwrap();
        assert_eq!((last.at, last.state.as_str(), last.channels), (77, "Failure", 0));

        let other = Event { kind: "recording.changed".to_string(), payload: "null".to_string(), timestamp: 1 };
        on_event(&other).unwrap();
        assert_eq!(last_update().unwrap().at, 77, "unchanged");
        assert!(
            on_event(&Event { kind: "playlist.update".to_string(), payload: "{".to_string(), timestamp: 1 }).is_err()
        );
    }

    #[test]
    fn metrics_report_cache_calls_and_the_last_refreshs_enriched_channels() {
        let settings = settings(r#"{"leagues":["epl"]}"#);
        mock::set_now(1_000);
        cache(&[league("epl", "football", &[], vec![fixture("a", "A", "B", 1, &[]), fixture("b", "C", "D", 2, &[])])]);
        on_event(&event(payload(), 1)).unwrap();
        let rows = collect(&settings);
        let shown: Vec<_> = rows.iter().map(|m| (m.key.as_str(), m.value.as_str(), m.unit.as_deref())).collect();
        assert_eq!(
            shown,
            [("fixtures_cached", "2", None), ("api_calls_today", "0", Some("calls")), ("channels_enriched", "6", None)]
        );
        assert_eq!(rows[0].label, "metric.fixtures_cached");
    }
}
