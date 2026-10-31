# @tuliprox/plugin-ui-types

TypeScript definitions for the tuliprox plugin UI bridge. A plugin's UI runs in
a sandboxed `<iframe>` (`sandbox="allow-scripts"`). The frame talks to tuliprox
through `window.tuliprox`, which is built on `postMessage`. This package
describes that object, the messages exchanged with the parent window, the slot
contexts, and the plugin DTOs a frame can see.

The package contains types only. It has no runtime code.

## Loading the bridge

The frame loads the host-provided client from tuliprox. The client defines
`window.tuliprox`:

```html
<script src="/plugins/_sdk/bridge.js"></script>
```

The client posts `ready` on load. The parent answers with `init`, and
`window.tuliprox.ready` resolves then.

The frame runs under the plugin's Content Security Policy, which allows scripts
from `'self'` only. Inline `<script>` blocks do not run. Put your code in a
separate file under the plugin's `ui/` directory and load it with a relative
`src`.

## Usage

`ui/sports-guide.html`:

```html
<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <link rel="stylesheet" href="sports-guide.css" />
  </head>
  <body>
    <h1 id="title"></h1>
    <script src="/plugins/_sdk/bridge.js"></script>
    <script src="sports-guide.js"></script>
  </body>
</html>
```

`ui/sports-guide.js`:

```js
const tp = window.tuliprox;

tp.ready.then(async () => {
  document.getElementById("title").textContent = tp.t("TITLE");

  try {
    const guide = await tp.rpc("guide", { days: 1 });
    render(guide);
  } catch (err) {
    // err is an RpcError: { status, error }
    tp.toast("error", `${err.status}: ${err.error}`);
  }
  tp.resize();
});
```

With TypeScript, import the type. `window.tuliprox` is already typed as `Tuliprox`
through the package's global declaration:

```ts
import type { Tuliprox } from "@tuliprox/plugin-ui-types";

const tp: Tuliprox = window.tuliprox;
```

`test/usage.ts` in this package touches every member of `Tuliprox` and every
message type.

## Version policy

The package version follows the host ABI:

- The **major** version is the host ABI major. Version `1.x.y` targets ABI 1,
  and the `tuliproxAbi` field in `package.json` records it. A change to the
  bridge protocol that breaks existing frames bumps the major version together
  with the ABI.
- A **minor** version adds members or message types that existing frames can
  ignore.
- A **patch** version fixes typing errors with no change to the wire shape.

## Status: not yet published

This package is `"private": true` in `package.json`, so `npm publish` refuses to
run. Publishing needs the owner's approval. When that is given, remove the
`private` field and publish from this directory. Until then, reference it from
the repository by path.
