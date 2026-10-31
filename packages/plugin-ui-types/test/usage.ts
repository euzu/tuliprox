// Compile-only example. Run `tsc -p packages/plugin-ui-types --noEmit`.
// It touches every window.tuliprox member, every message type, every slot
// context and every exported DTO. Nothing here runs.

import type {
  FrameToParentMessage,
  ParentToFrameMessage,
  PluginChannelContext,
  PluginContributionDto,
  PluginMetricDto,
  PluginProgrammeContext,
  PluginRpcExportDto,
  PluginUiDto,
  RpcError,
  Severity,
  SlotContextMap,
  SlotId,
  ThemeTokens,
  Tuliprox,
  TuliproxEvent,
} from "../index";

// Every member of window.tuliprox.
export async function frameApi(tp: Tuliprox): Promise<void> {
  await tp.ready;
  const pluginId: string = tp.pluginId;
  const slot: SlotId = tp.slot;
  const locale: string = tp.locale;
  const dir: "ltr" | "rtl" = tp.dir;
  const theme: ThemeTokens = tp.theme;
  const context: unknown = tp.context;

  const untyped: unknown = await tp.rpc("ping");
  const typed = await tp.rpc<{ count: number }>("list", { page: 1 });
  const count: number = typed.count;

  try {
    await tp.rpc("export-report");
  } catch (err) {
    const rpcError = err as RpcError;
    const status: number = rpcError.status;
    const message: string = rpcError.error;
    console.debug(status, message, untyped);
  }

  const severity: Severity = "info";
  tp.toast(severity, "Saved");
  tp.toast("success", "Saved");
  tp.toast("warning", "Check the input");
  tp.toast("error", "Failed");

  const greeting: string = tp.t("HELLO", { name: "Ada", count: 2 });
  const plain: string = tp.t("TITLE");

  tp.resize();
  tp.resize(320);

  const event: TuliproxEvent = "theme";
  const off: () => void = tp.on(event, () => {
    console.debug(pluginId, slot, locale, dir, theme, context, count, greeting, plain);
  });
  tp.on("init", () => undefined)();
  tp.on("locale", () => undefined);
  tp.on("context", () => undefined);
  off();
}

// Every frame -> parent message type.
export function postFrameMessages(post: (msg: FrameToParentMessage) => void): void {
  const ready: FrameToParentMessage = { tlpx: 1, type: "ready" };
  const rpcWithPayload: FrameToParentMessage = {
    tlpx: 1,
    type: "rpc",
    id: 1,
    method: "list",
    payload: { page: 1 },
  };
  const rpcWithoutPayload: FrameToParentMessage = {
    tlpx: 1,
    type: "rpc",
    id: 2,
    method: "ping",
  };
  const toast: FrameToParentMessage = {
    tlpx: 1,
    type: "toast",
    severity: "success",
    message: "Saved",
  };
  const resize: FrameToParentMessage = { tlpx: 1, type: "resize", height: 240 };
  [ready, rpcWithPayload, rpcWithoutPayload, toast, resize].forEach(post);
}

// Every parent -> frame message type, with an exhaustive switch.
export function handleParentMessage(msg: ParentToFrameMessage): string {
  switch (msg.type) {
    case "init":
      return `${msg.pluginId}:${msg.slot}:${msg.locale}:${msg.dir}:${Object.keys(msg.strings).length}`;
    case "locale":
      return `${msg.locale}:${msg.dir}`;
    case "theme":
      return Object.keys(msg.theme).join(",");
    case "context":
      return String(msg.context);
    case "rpc-result":
      if (msg.ok) {
        return `result ${String(msg.result)}`;
      }
      return `error ${msg.status} ${msg.error}`;
    default: {
      const exhaustive: never = msg;
      return exhaustive;
    }
  }
}

// One literal of each parent -> frame message.
export const parentMessages: ParentToFrameMessage[] = [
  {
    tlpx: 1,
    type: "init",
    pluginId: "sports-guide",
    slot: "menu",
    locale: "en",
    dir: "ltr",
    strings: { TITLE: "Sports guide" },
    theme: { "--tlpx-bg": "#101418" },
    context: null,
  },
  { tlpx: 1, type: "locale", locale: "es", dir: "ltr", strings: { TITLE: "Guia" } },
  { tlpx: 1, type: "theme", theme: { "--tlpx-fg": "#f0f0f0" } },
  { tlpx: 1, type: "context", context: null },
  { tlpx: 1, type: "rpc-result", id: 1, ok: true, result: { count: 3 } },
  { tlpx: 1, type: "rpc-result", id: 2, ok: false, status: 404, error: "no such method" },
];

// Slot contexts, one per slot that carries one.
const channel: PluginChannelContext = {
  virtualId: 42,
  name: "ESPN",
  title: "ESPN HD",
  group: "Sports",
  itemType: "live",
  epgChannelId: "espn.us",
  inputName: "provider-a",
};
const programme: PluginProgrammeContext = {
  title: "SportsCenter",
  start: 1767225600,
  stop: 1767229200,
  description: "Highlights",
  category: "Sports",
};
export const explorerContext: SlotContextMap["playlist-explorer-action"] = {
  channels: [channel],
};
export const detailContext: SlotContextMap["channel-detail-panel"] = { channel };
export const epgContext: SlotContextMap["epg-overlay"] = { channel, programme };
export const epgContextNoProgramme: SlotContextMap["epg-overlay"] = {
  channel,
  programme: null,
};
export const menuContext: SlotContextMap["menu"] = null;

// Compile-time check that SlotContextMap has exactly the SlotId keys.
type MissingSlots = Exclude<SlotId, keyof SlotContextMap>;
type ExtraSlots = Exclude<keyof SlotContextMap, SlotId>;
export const noMissingSlots: [MissingSlots] extends [never] ? true : false = true;
export const noExtraSlots: [ExtraSlots] extends [never] ? true : false = true;

// Plugin DTOs.
export const metric: PluginMetricDto = {
  key: "epg-probe.age",
  plugin: "epg-probe",
  label: "EPG age",
  value: "3 h",
  unit: "h",
  severity: "warn",
};

export const rpcExport: PluginRpcExportDto = {
  name: "list",
  kind: "read",
  mcp: false,
  description: "List the guide",
};

export const contribution: PluginContributionDto = {
  slot: "dashboard-card",
  title: "Sports guide",
  titleKey: "CARD_TITLE",
  icon: "tv",
  element: "ui/sports-guide.html",
};

export const pluginUi: PluginUiDto = {
  id: "sports-guide",
  name: "Sports guide",
  version: "1.0.0",
  state: "enabled",
  locales: ["en", "es"],
  configSection: false,
  configExtensions: [
    { section: "epg", schema: { type: "object", properties: {} } },
  ],
  configTab: {
    title: "Guide",
    icon: "tv",
    element: "ui/config.html",
  },
  contributions: [contribution],
  rpcExports: [rpcExport],
};

// Global augmentation: window.tuliprox is typed as Tuliprox.
export const globalTuliprox: Tuliprox | undefined =
  typeof window === "undefined" ? undefined : window.tuliprox;
