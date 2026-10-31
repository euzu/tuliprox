//! W5 fixture: the EPG and metadata hooks, driven by the plugin's settings.
//!
//! One component serves every id under `manifests/`; each manifest subscribes to
//! one hook. The scenario is the settings JSON the test puts in the plugin's
//! config entry:
//!
//! * `on-epg-loaded`: `load_from` / `load_to` rename the channel whose display
//!   name is `load_from` (ASCII case-insensitive) to the single name `load_to`.
//!   `load_bomb` (a number) returns that many channels instead, whatever it was
//!   sent, to exercise the host's cap on a sparse result.
//! * `transform-programmes`: `transform_category` replaces every programme's
//!   categories with that one value. `transform_bomb` (a number) returns that
//!   many copies of the first programme instead.
//! * `enrich-programmes`: `inject_title` / `inject_start` (unix seconds, 600 s
//!   long) add one synthetic programme; `inject_channel` limits it to one channel.
//! * `match-channels`: `match_map`, an object of playlist channel name -> guide
//!   id, answers a request whose channel `name` is a key (ASCII case-insensitive).
//!   The id is returned as given, even when it is not in the shortlist, so the
//!   host's own shortlist check is observable.
//! * `enrich-metadata`: `metadata_field` / `metadata_value` set one top-level
//!   string field of the metadata JSON.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:epg-probe/epg-probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    epg_enrich_hooks, epg_load_hooks, epg_match_hooks, epg_transform_hooks, lifecycle, metadata_hooks,
};
use serde_json::{Map, Value};
use std::sync::Mutex;
use tuliprox::plugin::{
    epg_types::{EpgCategory, EpgChannel, EpgProgramme},
    host,
    types::{Channel, LogLevel},
};

const INJECTED_LENGTH_SECS: i64 = 600;

static SETTINGS: Mutex<Option<Map<String, Value>>> = Mutex::new(None);

struct EpgProbe;

fn settings() -> Map<String, Value> {
    SETTINGS.lock().ok().and_then(|guard| guard.clone()).unwrap_or_default()
}

fn text(settings: &Map<String, Value>, key: &str) -> Option<String> {
    settings.get(key).and_then(Value::as_str).map(ToString::to_string)
}

impl lifecycle::Guest for EpgProbe {
    fn init(config: String) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
        if let Ok(mut guard) = SETTINGS.lock() {
            *guard = Some(parsed.as_object().cloned().unwrap_or_default());
        }
        host::log(LogLevel::Debug, "epg-probe initialised");
        Ok(())
    }
}

impl epg_load_hooks::Guest for EpgProbe {
    fn on_epg_loaded(_source: String, channels: Vec<EpgChannel>) -> Result<Vec<EpgChannel>, String> {
        let settings = settings();
        if let Some(count) = settings.get("load_bomb").and_then(Value::as_u64) {
            return Ok((0..count)
                .map(|index| EpgChannel { id: format!("bomb-{index}"), names: Vec::new(), icon: None })
                .collect());
        }
        let (Some(from), Some(to)) = (text(&settings, "load_from"), text(&settings, "load_to")) else {
            return Ok(Vec::new());
        };
        // Sparse: only the channels that change are returned.
        Ok(channels
            .into_iter()
            .filter(|channel| channel.names.iter().any(|name| name.eq_ignore_ascii_case(&from)))
            .map(|mut channel| {
                channel.names = vec![to.clone()];
                channel
            })
            .collect())
    }
}

impl epg_transform_hooks::Guest for EpgProbe {
    fn transform_programmes(_source: String, programmes: Vec<EpgProgramme>) -> Result<Vec<EpgProgramme>, String> {
        let settings = settings();
        if let (Some(count), Some(first)) = (settings.get("transform_bomb").and_then(Value::as_u64), programmes.first()) {
            return Ok((0..count).map(|_| first.clone()).collect());
        }
        let Some(category) = text(&settings, "transform_category") else {
            return Ok(programmes);
        };
        Ok(programmes
            .into_iter()
            .map(|mut programme| {
                programme.categories = vec![EpgCategory { value: category.clone(), lang: None }];
                programme
            })
            .collect())
    }
}

impl epg_enrich_hooks::Guest for EpgProbe {
    fn enrich_programmes(channel_id: String, mut programmes: Vec<EpgProgramme>) -> Result<Vec<EpgProgramme>, String> {
        let settings = settings();
        let Some(start) = settings.get("inject_start").and_then(Value::as_i64) else {
            return Ok(programmes);
        };
        if text(&settings, "inject_channel").is_some_and(|only| !only.eq_ignore_ascii_case(&channel_id)) {
            return Ok(programmes);
        }
        programmes.push(EpgProgramme {
            channel: channel_id,
            start,
            stop: start + INJECTED_LENGTH_SECS,
            title: Some(text(&settings, "inject_title").unwrap_or_else(|| "Injected".to_string())),
            description: None,
            categories: Vec::new(),
            catchup_id: None,
            icon: None,
            is_live: false,
            is_new: false,
            previously_shown: false,
            extra: Vec::new(),
        });
        Ok(programmes)
    }
}

impl epg_match_hooks::Guest for EpgProbe {
    fn match_channels(requests: Vec<epg_match_hooks::MatchRequest>) -> Result<Vec<Option<String>>, String> {
        let settings = settings();
        let map = settings.get("match_map").and_then(Value::as_object);
        Ok(requests
            .iter()
            .map(|request| {
                map.and_then(|map| {
                    map.iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&request.chan.name))
                        .and_then(|(_, id)| id.as_str().map(ToString::to_string))
                })
            })
            .collect())
    }
}

impl metadata_hooks::Guest for EpgProbe {
    fn enrich_metadata(_item: Channel, metadata_json: String) -> Result<Option<String>, String> {
        let settings = settings();
        let (Some(field), Some(value)) = (text(&settings, "metadata_field"), text(&settings, "metadata_value")) else {
            return Ok(None);
        };
        let mut metadata: Value = serde_json::from_str(&metadata_json).map_err(|err| err.to_string())?;
        let Some(object) = metadata.as_object_mut() else {
            return Err("metadata is not a JSON object".to_string());
        };
        object.insert(field, Value::String(value));
        serde_json::to_string(&metadata).map(Some).map_err(|err| err.to_string())
    }
}

export!(EpgProbe);
