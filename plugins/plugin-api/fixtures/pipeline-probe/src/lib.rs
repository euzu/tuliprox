//! W5 fixture: exercises every pipeline hook from one component.
//!
//! The component exports `playlist-hooks`, `source-hooks`, `sort-hooks`,
//! `sort-key-hooks`, `chno-hooks`, `dedupe-hooks` and `group-merge-hooks`. Each
//! manifest under `manifests/` subscribes to one scenario's hook(s); the
//! behaviour is chosen by the settings JSON the test hands the plugin entry:
//!
//! ```json
//! {
//!   "stage":  { "at": "after-filter", "drop_name_contains": "x", "title_suffix": "!", "group": "Other" },
//!   "loaded": { "drop_name_contains": "x", "title_suffix": "!", "group": "Other" },
//!   "reverse_after_sort": true,
//!   "sort_key": "name" | "reverse-name" | "name-length",
//!   "chno": "sequential:100" | "reverse:100" | "by-name-length",
//!   "dedupe": "keep-last" | "keep-first" | "keep-none" | "keep-all-reversed",
//!   "merge": { "Sports": ["Sport", "SPORTS"] } | "case-insensitive"
//! }
//! ```
//!
//! `stage` and `loaded` share one shape (`Rule`): drop channels whose name
//! contains `drop_name_contains`, append `title_suffix` to the title of the
//! others, move them to `group`. `stage.at` is the stage name that fires the
//! rule (`before-filter` ... `after-map`); the others do nothing. Every key is
//! optional, so one wasm serves all manifests.

wit_bindgen::generate!({
    path: "../../wit",
    inline: r#"
        package fixture:pipeline-probe;

        world probe {
            include tuliprox:plugin/base@1.0.0;
            export tuliprox:plugin/playlist-hooks@1.0.0;
            export tuliprox:plugin/source-hooks@1.0.0;
            export tuliprox:plugin/sort-hooks@1.0.0;
            export tuliprox:plugin/sort-key-hooks@1.0.0;
            export tuliprox:plugin/chno-hooks@1.0.0;
            export tuliprox:plugin/dedupe-hooks@1.0.0;
            export tuliprox:plugin/group-merge-hooks@1.0.0;
        }
    "#,
    world: "probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    chno_hooks, dedupe_hooks, group_merge_hooks, lifecycle,
    playlist_hooks::{self, Stage},
    sort_hooks, sort_key_hooks, source_hooks,
};
use serde_json::Value;
use tuliprox::plugin::{
    host,
    types::{Channel, ChannelChange},
};

struct Probe;

fn settings() -> Value { serde_json::from_str(&host::get_config()).unwrap_or(Value::Null) }

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> { value.get(key).and_then(Value::as_str) }

const fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::BeforeFilter => "before-filter",
        Stage::AfterFilter => "after-filter",
        Stage::BeforeRename => "before-rename",
        Stage::AfterRename => "after-rename",
        Stage::BeforeMap => "before-map",
        Stage::AfterMap => "after-map",
    }
}

/// Drop / retitle / regroup, per the `stage` or `loaded` settings object.
fn apply_rule(rule: &Value, channels: Vec<Channel>) -> Vec<ChannelChange> {
    let drop_marker = text(rule, "drop_name_contains");
    let suffix = text(rule, "title_suffix");
    let group = text(rule, "group");
    let mut changes = Vec::new();
    for mut channel in channels {
        if drop_marker.is_some_and(|marker| channel.name.contains(marker)) {
            changes.push(ChannelChange::Drop(channel.handle));
            continue;
        }
        let mut changed = false;
        if let Some(suffix) = suffix {
            channel.title.push_str(suffix);
            changed = true;
        }
        if let Some(group) = group {
            channel.group = group.to_string();
            changed = true;
        }
        if changed {
            changes.push(ChannelChange::Modified(channel));
        }
    }
    changes
}

impl lifecycle::Guest for Probe {
    fn init(_settings: String) -> Result<(), String> { Ok(()) }
}

impl playlist_hooks::Guest for Probe {
    fn on_stage(stage: Stage, _target: String, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
        let settings = settings();
        let Some(rule) = settings.get("stage") else {
            return Ok(Vec::new());
        };
        if text(rule, "at") != Some(stage_name(stage)) {
            return Ok(Vec::new());
        }
        Ok(apply_rule(rule, channels))
    }
}

impl source_hooks::Guest for Probe {
    fn on_playlist_loaded(_input_name: String, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
        let settings = settings();
        Ok(settings.get("loaded").map_or_else(Vec::new, |rule| apply_rule(rule, channels)))
    }
}

impl sort_hooks::Guest for Probe {
    fn after_sort(_target: String, channels: Vec<Channel>) -> Result<Vec<u32>, String> {
        let reverse = settings().get("reverse_after_sort").and_then(Value::as_bool).unwrap_or(false);
        let mut handles: Vec<u32> = channels.iter().map(|channel| channel.handle).collect();
        if reverse {
            handles.reverse();
        }
        Ok(handles)
    }
}

impl sort_key_hooks::Guest for Probe {
    fn sort_key(_target: String, channels: Vec<Channel>) -> Result<Vec<String>, String> {
        let settings = settings();
        let scheme = text(&settings, "sort_key").unwrap_or("name");
        Ok(channels
            .iter()
            .map(|channel| match scheme {
                "reverse-name" => channel.name.chars().rev().collect::<String>().to_lowercase(),
                "name-length" => format!("{:05}{}", channel.name.chars().count(), channel.name.to_lowercase()),
                _ => channel.name.to_lowercase(),
            })
            .collect())
    }
}

impl chno_hooks::Guest for Probe {
    fn assign_channel_numbers(_target: String, channels: Vec<Channel>) -> Result<Vec<(u32, u32)>, String> {
        let settings = settings();
        let scheme = text(&settings, "chno").unwrap_or("sequential:100");
        let (kind, start) = scheme.split_once(':').map_or((scheme, 100), |(kind, start)| (kind, start.parse().unwrap_or(100)));
        let total = u32::try_from(channels.len()).unwrap_or(u32::MAX);
        Ok(channels
            .iter()
            .enumerate()
            .map(|(position, channel)| {
                let position = u32::try_from(position).unwrap_or(u32::MAX);
                let chno = match kind {
                    "reverse" => start + (total - 1 - position),
                    "by-name-length" => u32::try_from(channel.name.chars().count()).unwrap_or(u32::MAX),
                    _ => start + position,
                };
                (channel.handle, chno)
            })
            .collect())
    }
}

impl dedupe_hooks::Guest for Probe {
    fn resolve_duplicates(_target: String, group: dedupe_hooks::DuplicateGroup) -> Result<Vec<u32>, String> {
        let settings = settings();
        let handles: Vec<u32> = group.channels.iter().map(|channel| channel.handle).collect();
        Ok(match text(&settings, "dedupe").unwrap_or("keep-first") {
            "keep-last" => handles.last().copied().into_iter().collect(),
            "keep-all-reversed" => handles.into_iter().rev().collect(),
            "keep-none" => Vec::new(),
            _ => handles.first().copied().into_iter().collect(),
        })
    }
}

impl group_merge_hooks::Guest for Probe {
    fn merge_groups(_target: String, groups: Vec<String>) -> Result<Vec<(String, Vec<String>)>, String> {
        let settings = settings();
        match settings.get("merge") {
            Some(Value::Object(map)) => Ok(map
                .iter()
                .map(|(canonical, members)| {
                    let members = members
                        .as_array()
                        .map(|members| members.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default();
                    (canonical.clone(), members)
                })
                .collect()),
            Some(Value::String(scheme)) if scheme == "case-insensitive" => {
                // The first-seen spelling of a title is canonical for the others.
                let mut merged: Vec<(String, Vec<String>)> = Vec::new();
                for group in groups {
                    match merged.iter_mut().find(|(canonical, _)| canonical.eq_ignore_ascii_case(&group)) {
                        Some((_, members)) => members.push(group),
                        None => merged.push((group.clone(), vec![group])),
                    }
                }
                Ok(merged.into_iter().filter(|(_, members)| members.len() > 1).collect())
            }
            _ => Ok(Vec::new()),
        }
    }
}

export!(Probe);
