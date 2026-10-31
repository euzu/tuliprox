// Vanilla TypeScript for the sandboxed menu frame (ui/menu.html).
//
// The frame loads ../../_sdk/bridge.js, which defines window.tuliprox. The
// types below describe the parts of that bridge this frame uses; the full
// reference is packages/plugin-ui-types in the tuliprox repository.
//
// Compile to ui/menu.js (ES2020, no bundler) and commit both files:
//   npx -p typescript tsc --strict --target es2020 --lib es2020,dom --noEmitOnError false ui/menu.ts
// Keep ui/menu.js in step with this file; the packaged plugin ships the .js.

type Severity = "info" | "success" | "warning" | "error";

interface TuliproxBridge {
  readonly ready: Promise<void>;
  readonly pluginId: string;
  readonly slot: string;
  readonly locale: string;
  readonly context: unknown;
  rpc(method: string, payload?: unknown): Promise<unknown>;
  toast(severity: Severity, message: string): void;
  t(key: string, params?: Record<string, string | number>): string;
  resize(height?: number): void;
  on(event: "init" | "locale" | "theme" | "context", callback: () => void): () => void;
}

declare global {
  interface Window {
    tuliprox: TuliproxBridge;
  }
}

export {};

const bridge = window.tuliprox;

function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) {
    throw new Error(`missing element #${id}`);
  }
  return element as T;
}

function render(): void {
  byId("title").textContent = bridge.t("menu.title");
  byId("hint").textContent = bridge.t("menu.hint");
  byId("slot").textContent = bridge.slot;
  byId("locale").textContent = bridge.locale;
  bridge.resize();
}

async function main(): Promise<void> {
  await bridge.ready;
  render();
  for (const event of ["init", "locale"] as const) {
    bridge.on(event, render);
  }
  byId("toast").addEventListener("click", () => bridge.toast("success", bridge.t("menu.toast")));
}

void main();
