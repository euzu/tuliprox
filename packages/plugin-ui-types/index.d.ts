/**
 * Type definitions for the tuliprox sandboxed-frame bridge (host ABI 1).
 *
 * A plugin UI frame loads `_sdk/bridge.js` (relative to `ui/`), which defines
 * `window.tuliprox`. This module describes that object, the messages exchanged
 * with the parent window, the slot contexts, and the plugin DTOs a frame can
 * see. Wire shapes mirror `shared/src/model/plugins_dto.rs`.
 */

/**
 * An extension slot a frame can be mounted into. Serialized kebab-case
 * (`PluginSlotDto`).
 */
export type SlotId =
  | "menu"
  | "config-section"
  | "config-tab"
  | "source-editor-tile"
  | "dashboard-card"
  | "playlist-explorer-action"
  | "channel-detail-panel"
  | "epg-overlay"
  | "setup-wizard-step";

/** Text direction of the plugin locale. */
export type TextDirection = "ltr" | "rtl";

/** Toast severity, used by `Tuliprox.toast` and the `toast` message. */
export type Severity = "info" | "success" | "warning" | "error";

/**
 * Theme values pushed by the parent: CSS custom-property names mapped to their
 * values. Applied to `:root` by the bridge.
 */
export type ThemeTokens = Record<string, string>;

/**
 * Rejection value of `Tuliprox.rpc`, and the failure answer of an `rpc-result`
 * message. `status` is the HTTP status of the host rpc route (for example 404
 * for an unexported method, 409 for a disabled plugin, 422 for a payload that
 * fails `input_schema`, 429 when rate limited, 502 or 503 on a guest failure),
 * or a bridge-side status when the parent refuses before calling the host.
 */
export interface RpcError {
  status: number;
  error: string;
}

/**
 * The channel carried in the `context` of the channel-carrying slots. It has no
 * URLs: stream and catch-up URLs embed provider credentials.
 */
export interface PluginChannelContext {
  /** Virtual channel id. */
  virtualId: number;
  name: string;
  title: string;
  group: string;
  /** `live`, `movie`, `series`, ... */
  itemType: string;
  epgChannelId?: string;
  inputName?: string;
}

/** The focused programme of the `epg-overlay` slot. */
export interface PluginProgrammeContext {
  title: string;
  /** Unix seconds. */
  start: number;
  /** Unix seconds. */
  stop: number;
  description?: string;
  category?: string;
}

/**
 * The `context` value per slot. Slots without a subject carry `null`.
 */
export interface SlotContextMap {
  menu: null;
  "config-section": null;
  "config-tab": null;
  "source-editor-tile": null;
  "dashboard-card": null;
  /** Channels selected in the playlist explorer. */
  "playlist-explorer-action": { channels: PluginChannelContext[] };
  "channel-detail-panel": { channel: PluginChannelContext };
  "epg-overlay": {
    channel: PluginChannelContext;
    programme: PluginProgrammeContext | null;
  };
  "setup-wizard-step": null;
}

/**
 * Lifecycle state of a plugin (`PluginStateDto`, snake_case on the wire).
 */
export type PluginState =
  | "discovered"
  | "invalid"
  | "pending_approval"
  | "approved"
  | "loaded"
  | "enabled"
  | "disabled"
  | "failed";

/** Accent of a dashboard metric card (`PluginMetricSeverityDto`). */
export type PluginMetricSeverity = "info" | "warn" | "error";

/**
 * One `collect-metrics` value, as shown on the dashboard
 * (`PluginMetricDto`).
 */
export interface PluginMetricDto {
  /** Namespaced by the host: `<plugin-id>.<key>`. */
  key: string;
  plugin: string;
  /** Display label, or a key of the plugin's `ui/locales`. */
  label: string;
  /** Preformatted by the plugin. */
  value: string;
  unit?: string;
  severity?: PluginMetricSeverity;
}

/** A `ui.rpc_exports` entry (`PluginRpcExportDto`). */
export interface PluginRpcExportDto {
  name: string;
  /** `write` needs `SystemWrite` on the host and is never an MCP tool. */
  kind: "read" | "write";
  /** The manifest offers this export as an MCP tool. */
  mcp: boolean;
  /** Sanitized: control characters stripped, at most 512 characters. */
  description?: string;
}

/** An entry of `ui.contributions`: a sandboxed frame in a slot (`PluginContributionDto`). */
export interface PluginContributionDto {
  slot: SlotId;
  /** Literal fallback title. */
  title: string;
  titleKey?: string;
  icon?: string;
  /** Package-relative frame document, e.g. `ui/sports-guide.html`. */
  element: string;
}

/**
 * A `ui.config_extensions[]` entry: fields shown inside a built-in config
 * section (`PluginConfigExtensionDto`).
 */
export interface PluginConfigExtensionDto {
  section: string;
  /** Inline JSON Schema fragment (the `SchemaForm` subset). */
  schema: unknown;
}

/**
 * A `ui.config_tab`. Exactly one of `schema` or `element` is set
 * (`PluginConfigTabDto`).
 */
export interface PluginConfigTabDto {
  title?: string;
  titleKey?: string;
  icon?: string;
  /** The tab's JSON Schema fragment. */
  schema?: unknown;
  /** Package-relative frame document. */
  element?: string;
}

/**
 * One plugin in `GET /api/v1/plugins/ui`: everything the web UI renders for it
 * (`PluginUiDto`).
 */
export interface PluginUiDto {
  id: string;
  name: string;
  version: string;
  state: PluginState;
  failure?: string;
  /** `ui.locales`; `en` is the fallback. */
  locales: string[];
  /** Whether `ui.config_section` is set. */
  configSection: boolean;
  configExtensions: PluginConfigExtensionDto[];
  configTab?: PluginConfigTabDto;
  contributions: PluginContributionDto[];
  rpcExports: PluginRpcExportDto[];
}

/**
 * Message from a frame to its parent. Every message is a plain object with
 * `tlpx: 1`. The parent answers `ready` with `init`.
 */
export type FrameToParentMessage =
  /** Sent once on load. */
  | { tlpx: 1; type: "ready" }
  /** A request to call a `ui.rpc_exports` method. `id` is unique per frame. */
  | { tlpx: 1; type: "rpc"; id: number; method: string; payload?: unknown }
  /** A toast, routed into the host toastr (rate-limited). */
  | { tlpx: 1; type: "toast"; severity: Severity; message: string }
  /** The frame's content height in CSS pixels. The parent clamps it to 80-2000. */
  | { tlpx: 1; type: "resize"; height: number };

/**
 * Message from the parent to a frame. Every message is a plain object with
 * `tlpx: 1`.
 */
export type ParentToFrameMessage =
  /** The answer to every `ready`. */
  | {
      tlpx: 1;
      type: "init";
      pluginId: string;
      slot: SlotId;
      locale: string;
      dir: TextDirection;
      /** The plugin's resolved locale map. */
      strings: Record<string, string>;
      theme: ThemeTokens;
      context: unknown;
    }
  /** A locale change. */
  | {
      tlpx: 1;
      type: "locale";
      locale: string;
      dir: TextDirection;
      strings: Record<string, string>;
    }
  /** A theme change. */
  | { tlpx: 1; type: "theme"; theme: ThemeTokens }
  /** A context change, such as a new channel selection. */
  | { tlpx: 1; type: "context"; context: unknown }
  /** The answer to an `rpc` message, on success. */
  | { tlpx: 1; type: "rpc-result"; id: number; ok: true; result: unknown }
  /** The answer to an `rpc` message, on failure. */
  | ({ tlpx: 1; type: "rpc-result"; id: number; ok: false } & RpcError);

/** The events `Tuliprox.on` can subscribe to. */
export type TuliproxEvent = "init" | "locale" | "theme" | "context";

/**
 * The frame-side client that `_sdk/bridge.js` defines as
 * `window.tuliprox`. It is built on `parent.postMessage`, and the frame never
 * holds a token.
 */
export interface Tuliprox {
  /** Resolves on the first `init`. */
  readonly ready: Promise<void>;
  readonly pluginId: string;
  readonly slot: SlotId;
  readonly locale: string;
  readonly dir: TextDirection;
  /** The current theme tokens. */
  readonly theme: ThemeTokens;
  /** The current slot context. Narrow it with `SlotContextMap[slot]`. */
  readonly context: unknown;
  /**
   * Calls a `ui.rpc_exports` method through the parent. Resolves with the
   * method's result. Rejects with an `RpcError`.
   */
  rpc<T = unknown>(method: string, payload?: unknown): Promise<T>;
  /** Shows a toast, prefixed with the plugin name by the host. */
  toast(severity: Severity, message: string): void;
  /**
   * Resolves a key in the plugin's locale map. `{name}` placeholders are
   * replaced from `params`. Returns the key itself when it is missing.
   */
  t(key: string, params?: Record<string, string | number>): string;
  /**
   * Reports the frame height. Defaults to `document.documentElement.scrollHeight`.
   */
  resize(height?: number): void;
  /** Subscribes to an event. Returns the unsubscribe function. */
  on(event: TuliproxEvent, cb: () => void): () => void;
}

declare global {
  interface Window {
    /** Defined by `_sdk/bridge.js` inside a plugin UI frame. */
    tuliprox: Tuliprox;
  }
}
