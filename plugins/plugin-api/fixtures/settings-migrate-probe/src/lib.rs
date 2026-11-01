//! W8 fixture: a settings schema at version 2 with a `settings.migrate` export.
//!
//! Version 1 stored `{label, token}`. Version 2 (`config-schema.json`) wants
//! `{group, retries, api_key}`, so the v1 -> v2 step renames `label` to `group`,
//! moves the secret `token` to `api_key`, and adds `retries: 3`.
//!
//! Two magic labels exercise the failure paths of the host:
//!
//! * `fail`: the step returns `err`;
//! * `invalid`: the step returns settings the v2 schema rejects (`group` is a
//!   number).
//!
//! `playlist.after-filter` moves every channel to `group` (and tags the title
//! `keyed` when an `api_key` is set), so a test sees which settings the plugin
//! ran on. `init` fails when the settings do not parse as version 2.

use serde_json::{json, Value};
use tuliprox_plugin_sdk::prelude::*;

/// The v1 -> v2 step: `label` becomes `group`, `token` becomes `api_key`.
fn v1_to_v2(mut value: Value) -> Result<Value, String> {
    let object = value.as_object_mut().ok_or("the settings are not an object")?;
    let label = object.remove("label");
    if label.as_ref().and_then(Value::as_str) == Some("fail") {
        return Err("cannot migrate this label".to_string());
    }
    if label.as_ref().and_then(Value::as_str) == Some("invalid") {
        object.insert("group".to_string(), json!(5));
    } else if let Some(label) = label {
        object.insert("group".to_string(), label);
    }
    if let Some(token) = object.remove("token") {
        object.insert("api_key".to_string(), token);
    }
    object.entry("retries").or_insert(json!(3));
    Ok(value)
}

#[tuliprox_plugin]
mod plugin {
    use super::*;

    #[tuliprox_plugin_settings]
    pub struct Settings {
        group: String,
        #[serde(default)]
        #[allow(dead_code)]
        retries: u32,
        #[serde(default)]
        api_key: Secret<String>,
    }

    pub struct Probe;

    impl PlaylistHooks for Probe {
        fn on_stage(stage: Stage, _target: &str, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
            if !matches!(stage, Stage::AfterFilter) {
                return Ok(Vec::new());
            }
            let settings = Settings::current()?;
            let keyed = !settings.api_key.expose().is_empty();
            Ok(channels
                .into_iter()
                .map(|mut channel| {
                    channel.group.clone_from(&settings.group);
                    if keyed {
                        channel.title.push_str(" keyed");
                    }
                    ChannelChange::Modified(channel)
                })
                .collect())
        }
    }

    impl SettingsMigration for Probe {
        fn migrate(from_version: u32, settings: &str) -> Result<String, String> {
            Migrations::new().step(1, v1_to_v2).run(from_version, settings)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_renames_moves_and_defaults() {
        let migrated = Migrations::new().step(1, v1_to_v2).run(1, r#"{"label":"News","token":"${env:T}"}"#).unwrap();
        let value: Value = serde_json::from_str(&migrated).unwrap();
        assert_eq!(value, json!({"group": "News", "api_key": "${env:T}", "retries": 3}));
    }
}
