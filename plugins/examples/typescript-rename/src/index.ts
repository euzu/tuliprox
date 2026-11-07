// TypeScript example plugin: rename one playlist group in `playlist.after-filter`.
// Built with ComponentizeJS (see README.md). The WIT contract is plugins/plugin-api/wit.

import { getConfig } from "tuliprox:plugin/host@1.0.0";
import type { Stage } from "../generated/interfaces/tuliprox-plugin-playlist-hooks.js";
import type { Channel, ChannelChange } from "../generated/interfaces/tuliprox-plugin-types.js";
import { parseSettings, renameGroup } from "./rename.ts";

// A WIT `result<_, string>` is a plain return for ok and a thrown string for err.
export const lifecycle = {
  init(_settings: string): void {
    parseSettings(getConfig());
  },
};

export const playlistHooks = {
  onStage(stage: Stage, _target: string, channels: Channel[]): ChannelChange[] {
    // The manifest lists only playlist.after-filter, but the host may pass other stages of this interface.
    if (stage !== "after-filter") {
      return [];
    }
    return renameGroup(channels, parseSettings(getConfig()));
  },
};
