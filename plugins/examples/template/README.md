# My plugin

A tuliprox plugin that renames one playlist group in the `playlist.after-filter`
stage. It also has one sandboxed menu page and a settings section.

## Layout

```text
manifest.yml          plugin manifest: id, hooks, UI, resources
config-schema.json    settings form (rename_from, rename_to, api_token)
src/lib.rs            the hook (PlaylistHooks::on_stage), the settings, and a mock-host unit test
ui/menu.html          menu frame; ui/menu.ts is its source, ui/menu.js the compiled file
ui/locales/*.json     translations (en is the fallback)
.github/workflows/    CI: build the component and assemble the .tlpx
```

## Build and package

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

Package it for upload. The archive has `manifest.yml` at its root:

```bash
bin/package-plugin.sh <this-directory>    # from the tuliprox repository
```

or zip the directory by hand, with the same root layout and without the TypeScript sources:

```bash
zip -X -D -r my-plugin.tlpx manifest.yml plugin.wasm config-schema.json ui -x 'ui/*.ts'
```

`plugin.wasm` is needed because `hooks:` is not empty. Add `rpc` to the zip
command only when `ui.rpc_exports` is used.

## Tests

`cargo test` runs the unit test in `src/lib.rs` on the host. It uses the SDK's
mock host, so no tuliprox server is needed.

## Install

Upload the `.tlpx` in the Plugin Manager, review the requested capabilities,
approve it, then enable it. Or unpack it into `config/plugins/<id>/` on the
server. A plugin is not active until the admin approves it and enables it.

## Settings

- `rename_from`: the exact group name to match. Empty disables the rename.
- `rename_to`: the new group name.
- `api_token`: a write-only secret. Nothing in this template reads it. The host
  masks it when the settings are read back and redacts it from logs.

## Changing the menu page

Edit `ui/menu.ts`, compile it to `ui/menu.js` (see the header of `ui/menu.ts`),
and repackage. The frame must not use inline scripts: the sandbox CSP allows
scripts from its own origin only, and it blocks network requests from the frame.

## Before you ship

- Replace the id, name, author, and description in `manifest.yml`.
- Commit `Cargo.lock` and build with `--locked`.
- The `tuliprox-plugin-sdk` dependency is a path to a tuliprox checkout. Before
  you share this crate outside that checkout, switch it to a published or git
  dependency.
