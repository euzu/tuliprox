# Custom UI

A plugin can add its own pages and panels to the admin web UI. Each one is an HTML document, with its scripts, in a
sandboxed frame. The plugin's JavaScript never runs in the admin page.

## Sandboxed frames

A frame is an `<iframe sandbox="allow-scripts" referrerpolicy="no-referrer">`, with no `allow-same-origin`
(`frontend/src/app/components/plugins/plugin_frame.rs`). It has an opaque origin. It cannot read the admin page's
storage, cookies or DOM, and its requests carry no tuliprox credentials.

The frame document is served by the host at `{web_ui_path}/plugins/{id}/ui/{path}`. The route needs no authentication,
because an iframe `src` carries no token. It is safe because everything it serves runs in an opaque origin. It answers
`404` for an unknown id, a plugin that is not `enabled`, an unsafe path, a file type that is not on the allowlist, and
any read error. The allowlist is `html`, `css`, `js`, `mjs`, `json`, `svg`, `png`, `jpg`, `jpeg`, `gif`, `webp`, `ico`,
`woff`, `woff2` and `txt`. Every response carries the content security policy below, copied from
`backend/plugins/src/ui_assets.rs` (`FRAME_CSP`). It also carries `X-Content-Type-Options: nosniff`, a
`Referrer-Policy` of `no-referrer`, a `Cross-Origin-Resource-Policy` of `cross-origin`, and `Cache-Control: no-cache`:

```text
default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'none'; frame-ancestors 'self'; sandbox allow-scripts
```

What the policy means in practice:

- Scripts load from the plugin's own origin only. Inline scripts are blocked. The template loads `menu.js` from a file.
- `connect-src 'none'` blocks every `fetch`, `XMLHttpRequest` and `WebSocket` from the frame. All data goes through the
  bridge.
- `frame-ancestors 'self'` allows the frame only inside the admin page.
- The `sandbox` directive also applies when the document is opened as a top-level page, so it gets an opaque origin too.

Each file served is at most 16 MiB (`MAX_ASSET_BYTES`). Only files under the package's `ui/` directory are served.

Navigation is not covered by the policy. A frame can still leak what it holds by navigating itself. For that reason
slot contexts never carry stream or catch-up URLs. See [Security Model](./security.md).

## Declaring a frame

Frames are declared in the manifest under `ui.contributions`. The template declares one menu page:

```yaml
ui:
  contributions:
    - slot: menu
      title: My plugin
      title_key: menu.title
      element: ui/menu.html
```

`element` is the frame document, relative to the package root. The package must contain that file.

## Slots

A contribution goes in one of nine slots. The slot ids are those of `SlotId` in `packages/plugin-ui-types/index.d.ts`,
and the same names are used in the manifest:

| Slot | Where it shows |
| --- | --- |
| `menu` | A page in the main menu |
| `config-section` | The plugin's settings form |
| `config-tab` | A tab on the config page. Each contribution is its own tab, next to the `ui.config_tab` tab if the plugin also has one |
| `source-editor-tile` | A tile in the source editor |
| `dashboard-card` | A card on the dashboard |
| `playlist-explorer-action` | An action on the selected channels of the playlist explorer |
| `channel-detail-panel` | A panel in the series view of the playlist explorer; for live and VOD channels, an entry in the channel's menu that opens the panel in a dialog |
| `epg-overlay` | An overlay on the EPG, for the focused programme |
| `setup-wizard-step` | Accepted but never shown: the setup wizard runs before any plugin is loaded. The install review warns about it |

Slots with no subject send `null` as their context.

## Contexts

The frame's `tuliprox.context` value depends on the slot. The types are in `packages/plugin-ui-types/index.d.ts`:

- `channel-detail-panel`: `{ channel }`
- `epg-overlay`: `{ channel, programme }`, where `programme` is `null` when no programme is focused
- `playlist-explorer-action`: `{ channels }`, an array of the selected channels
- every other slot: `null`

A channel context has `virtualId`, `name`, `title`, `group`, `itemType`, and optionally `epgChannelId` and `inputName`.
A programme context has `title`, `start` and `stop` in Unix seconds, and optionally `description` and `category`.
Neither carries a URL.

## Bridge API

tuliprox serves the bridge next to each plugin's own UI files, at `{web_ui_path}/plugins/<id>/ui/_sdk/bridge.js`, so a
page at the root of `ui/` loads it as `_sdk/bridge.js` under any `web_ui_path`. A page in a subfolder uses
`../_sdk/bridge.js`. The name `ui/_sdk/` is reserved: a package that contains it is refused. The template's
`ui/menu.html` does this:

```html
<script src="_sdk/bridge.js"></script>
<script src="menu.js"></script>
```

`window.tuliprox` exposes these members, from `backend/app/src/api/endpoints/plugin_bridge.js`. The frame can only use
what the parent has sent by the time `ready` resolves.

| Member | Purpose |
| --- | --- |
| `ready` | A promise that resolves when the parent has sent `init` |
| `pluginId`, `slot`, `locale`, `dir`, `theme`, `context` | The values the parent sent |
| `rpc(method, payload)` | Calls a manifest-declared `rpc_exports` method. Resolves with the result |
| `toast(severity, message)` | Shows a toast. Severity is `info`, `success`, `warning` or `error` |
| `t(key, params)` | Looks up a string in the plugin's locale. `{name}` placeholders take `params` |
| `resize(height?)` | Sets the frame height. Without an argument it reports the document height. The bridge also does this by itself when the document size changes |
| `on(event, callback)` | Listens to `init`, `locale`, `theme` or `context`. Returns an unsubscribe function |

The bridge posts messages with the envelope `{ tlpx: 1, type, ... }`. The message set is:

- Frame to parent: `ready`, `rpc` with `id`, `method` and `payload`, `toast` with `severity` and `message`, `resize`
  with `height`.
- Parent to frame: `init`, `locale`, `theme`, `context`, and `rpc-result` with `id`, `ok` and `result` or `status` and
  `error`.

The bridge reads only messages whose source is the frame's parent window. It answers `rpc-result` only for ids it issued.

Calls from the frame go to the parent, which makes the authenticated request for the frame. The frame never holds a
token. The parent calls `POST /api/v1/plugins/{id}/rpc/{method}` for a method the manifest declares
(`frontend/src/services/plugin_ui_service.rs`).

The parent applies these limits before it sends anything (`plugin_frame.rs`):

- A message from a frame is not parsed above 128 KiB.
- A method that is not in the plugin's `rpc_exports` is answered at once with status `404`.
- An rpc payload is at most 64 KiB.
- At most 8 calls are in flight, and a frame may send 5 calls per second with a burst of 10. Over that the frame gets
  status `429`, `rate limit exceeded`.
- An error text handed to the frame is at most 500 characters.

## Toasts and resize

Toasts pass through the parent and are shown as `<plugin name>: <message>`. The message is plain text with control
characters dropped (newlines are kept) and is cut at 300 characters. Toasts are rate limited per plugin: a burst of 3,
then one every 2 seconds. A toast over the limit is dropped (`frontend/src/services/toastr_service.rs`).

The frame reports its height with `resize`. The parent clamps the value to 80 to 2000 CSS pixels. A frame starts at
200 pixels. The theme tokens the frame receives are a fixed list of 22 CSS custom properties, not the whole computed
style (`THEME_TOKENS` in `plugin_frame.rs`).

## Localisation

Locale files are flat JSON objects of strings in `ui/locales/<lang>.json`. For example, `plugins/examples/template/ui/locales/en.json`:

```json
{
  "menu.title": "My plugin",
  "menu.hint": "This page is served by the plugin inside a sandboxed frame.",
  "menu.toast": "Hello from my plugin"
}
```

- The manifest lists the languages in `ui.locales`. The fallback language is `en`, and it must be present.
- A `title_key` in a contribution refers to a key in these files.
- A locale file is at most 1 MiB.
- The bridge sends the matching locale to the frame at `init` and `locale`.

## rpc_exports

A frame can call the plugin only through methods the manifest declares under `ui.rpc_exports`. Each entry has a `name`
(kebab-case, at most 64 characters), a `kind` of `read` or `write`, and an optional `mcp` flag and `description`.

```yaml
ui:
  rpc_exports:
    - name: get-fixtures
      kind: read
      mcp: true
      description: Upcoming fixtures for the tracked leagues.
      input_schema: rpc/get-fixtures.schema.json
```

This example is from `plugins/examples/sports-enricher/manifest.yml`.

- A `write` method needs the `SystemWrite` permission on the calling request.
- `input_schema` is required when `mcp: true`. Its file is validated with the same schema subset as settings, and the
  payload is checked against it before the plugin is called.
- The description is shown in the install review, with control characters removed and at most 512 characters kept.
- The request body is at most 64 KiB and the result at most 256 KiB. A result that contains a resolved secret is
  redacted.
- The host checks a call in this order and answers: unknown plugin `404`, plugin not `enabled` `409`, method not
  declared `404`, `write` method without `SystemWrite` `403`, payload too large `413`, rate limit exceeded `429` with
  `Retry-After` (10 per second, burst 20, per plugin), payload not JSON `400`, payload fails `input_schema` `422`
  (`backend/plugins/src/rpc.rs`).
- A call never counts against the plugin's circuit breaker, so a UI cannot disable a plugin by clicking.

See [Events, Notifications and Providers](./events-notifications-inputs.md#mcp-tools) for the MCP side.

## Types

The TypeScript types are in `packages/plugin-ui-types` (`@tuliprox/plugin-ui-types` 1.0.0, `index.d.ts`). The package
is marked `private` in its `package.json`, so it is not published to npm. Use it from this repository, for example by
copying `index.d.ts`. It has types only and no runtime code. It describes the bridge object, the message envelopes, the
slot contexts and the DTOs.

The template's `ui/menu.ts` declares the few bridge types it uses inline and does not import the package. It is the
source of the compiled `ui/menu.js`. Compile it before packaging, since the packaging script ships the `.js` and not
the `.ts` (see [Packaging](./packaging.md)). The command is in the header of `plugins/examples/template/ui/menu.ts`:

```sh
npx -p typescript tsc --strict --target es2020 --lib es2020,dom --noEmitOnError false ui/menu.ts
```
