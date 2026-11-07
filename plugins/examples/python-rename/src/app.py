"""Python example plugin: rename one playlist group in `playlist.after-filter`.

UNVERIFIED: this file was written against plugin-api/wit but has NOT been built or run.
The generated binding names (the `wit_world` module) depend on the componentize-py
version; check them with `componentize-py -d ../../plugin-api/wit -w playlist-plugin bindings gen`
before relying on the imports below.
"""

import json

from wit_world import exports
from wit_world.imports import host


def _settings():
    raw = host.get_config()
    return json.loads(raw) if raw else {}


class PlaylistHooks(exports.PlaylistHooks):
    def on_stage(self, stage, target, channels):
        # The manifest lists only playlist.after-filter; ignore any other stage.
        if str(stage) != "after-filter":
            return []
        settings = _settings()
        source = settings.get("rename_from", "")
        target_name = settings.get("rename_to", "")
        if not source or source == target_name:
            return []
        changes = []
        for channel in channels:
            if channel.group == source:
                channel.group = target_name
                changes.append(exports.playlist_hooks.ChannelChange_Modified(channel))
        return changes


class Lifecycle(exports.Lifecycle):
    def init(self, settings):
        _settings()
        return None
