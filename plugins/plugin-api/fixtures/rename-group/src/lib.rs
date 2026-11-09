//! Fixture: renames the group `News` to `Plugin News` in `after-filter`.
//!
//! Exercises the contract surface the rename needs: `lifecycle.init`, the sparse
//! `modified` result, handle echo, and a host `log` call.

wit_bindgen::generate!({
    path: "../../wit",
    world: "playlist-plugin",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    playlist_hooks::{self, Stage},
};
use tuliprox::plugin::{
    host,
    types::{ChannelChange, LogLevel},
};

const FROM: &str = "News";
const TO: &str = "Plugin News";

struct RenameGroup;

impl lifecycle::Guest for RenameGroup {
    fn init(_settings: String) -> Result<(), String> {
        host::log(LogLevel::Debug, "rename-group initialised");
        Ok(())
    }
}

impl playlist_hooks::Guest for RenameGroup {
    fn on_stage(
        stage: Stage,
        _target: String,
        channels: Vec<tuliprox::plugin::types::Channel>,
    ) -> Result<Vec<ChannelChange>, String> {
        if !matches!(stage, Stage::AfterFilter) {
            return Ok(Vec::new());
        }
        Ok(channels
            .into_iter()
            .filter(|channel| channel.group == FROM)
            .map(|mut channel| {
                channel.group = TO.to_string();
                ChannelChange::Modified(channel)
            })
            .collect())
    }
}

export!(RenameGroup);
