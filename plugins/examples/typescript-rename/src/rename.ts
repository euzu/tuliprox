import type { Channel, ChannelChange } from "../generated/interfaces/tuliprox-plugin-types.js";

/** The plugin's settings (config-schema.json). Both fields default to empty, which disables the rename. */
export interface Settings {
  rename_from?: string;
  rename_to?: string;
}

export function parseSettings(raw: string): Settings {
  return raw ? (JSON.parse(raw) as Settings) : {};
}

/** Sparse result: only the channels whose group changed come back, as `modified`. */
export function renameGroup(channels: Channel[], settings: Settings): ChannelChange[] {
  const from = settings.rename_from ?? "";
  const to = settings.rename_to ?? "";
  if (!from || from === to) {
    return [];
  }
  return channels
    .filter((channel) => channel.group === from)
    .map((channel) => ({ tag: "modified", val: { ...channel, group: to } }));
}
