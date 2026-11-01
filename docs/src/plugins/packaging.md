# Packaging and Publishing

This page covers the `.tlpx` package, the script that builds it, and a checklist before you publish a plugin.

## The package layout

The upload route accepts a zip archive. The layout and the checks below are those of `backend/plugins/src/install.rs` and
`backend/plugins/src/manifest/package.rs`.

| Entry | When |
| --- | --- |
| `manifest.yml` | Always, at the zip root. No nested top directory |
| `plugin.wasm` | When the manifest's `hooks:` list is not empty |
| `config-schema.json` | When the manifest names a schema, or the file exists |
| `ui/locales/<lang>.json` | For each language in `ui.locales` |
| Every file the manifest names | Schema, element and `input_schema` paths |
| `ui/**` | Every file under `ui/`, except TypeScript sources |
| `rpc/*.json` | Input schemas of `rpc_exports` |

Limits enforced at upload:

- The request body is at most 32 MiB.
- At most 512 entries.
- At most 64 MiB inflated in total, and 32 MiB for one entry.
- No symbolic links, no directory entries without a trailing slash, no duplicate names, no `..` or absolute paths, and no
  backslashes.
- A manifest with hooks and no `plugin.wasm` is refused: ``hooks are listed in manifest.yml but plugin.wasm is missing``.
  The component's exports must match the manifest's `hooks:` exactly. An export the host does not know is ignored with a
  warning.
- A schema or locale file is at most 1 MiB.

The plugin id is read from the manifest before anything else is trusted. It must match `^[a-z][a-z0-9-]{2,40}$`. The ids
`staging` and `metrics` are reserved. A `422` answer to the upload lists every problem found.

An upload is staged in `<storage dir>/plugins/staging/` and installed into `<config dir>/plugins/<id>/` only when the
admin approves it. If the id is already installed, the review says so (`replaces`).

## The packaging script

`bin/package-plugin.sh` builds and packages a plugin directory:

```sh
bin/package-plugin.sh <plugin-dir> [--wasm FILE] [--out FILE.tlpx]
```

- With no `--wasm`, and a manifest that lists hooks, the script runs `cargo build --release --target wasm32-wasip2` on
  the plugin's `Cargo.toml`. It adds `--locked` when a `Cargo.lock` exists.
- `--wasm FILE` packages an existing component instead of building one.
- The default output is `target/plugin-examples/<id>.tlpx` under the repository.
- A referenced file that is missing or is a symbolic link fails the run.
- TypeScript sources (`*.ts`, `*.tsx`, `*.tsbuildinfo` and `tsconfig*.json`) are left out. The compiled `.js` ships.
- The build output goes to `target/plugin-examples/build` under the repository, not into the plugin directory.
- The manifest is read by a small line-based scanner, not a YAML library. It handles the shapes the template and the
  examples use: top-level `id:` and `hooks:`, `locales:` as a flow or block list, and scalar `schema:`, `element:` and
  `input_schema:` paths. A manifest written another way can be refused by the script even when the host would accept
  it. Then package by hand.
- Python 3 runs the script's helper code with `-I`. Python 3.8 or later is needed.

The script's header in `bin/package-plugin.sh` describes the same layout. `bin/new-plugin.sh <id> <dest-dir>` renders
the template into a new directory first. See the [Quickstart](./quickstart.md).

## Packaging by hand

The template's `README.md` gives a zip command with the same layout:

```sh
zip -X -D -r my-plugin.tlpx manifest.yml plugin.wasm config-schema.json ui -x 'ui/*.ts'
```

Add `rpc` to the command only when `ui.rpc_exports` is used. Unlike the script, `zip` does not read the manifest, so
you must list every file it names, and you must leave out `*.tsbuildinfo` and `tsconfig*.json` yourself.

## Installing

Upload the `.tlpx` through the Plugin Manager, or with `POST /api/v1/plugins`. Then approve and enable it. The
[Quickstart](./quickstart.md) gives the commands.

You can also unpack the package into `<config dir>/plugins/<id>/` and write a `plugins.plugins[]` entry in `config.yml`.
That path skips the review: an enabled entry with no recorded approval approves the plugin on first sight. See
[Security Model](./security.md#trust-on-first-use).

## Updating and rolling back

Uploading a package whose `id` is already installed is an update. The package goes through the same validation as a new
one and waits in staging. The plugin list shows "update available for review". The review marks the capabilities the
update adds, lists the ones it no longer asks for, and says if MCP exposure was approved for the installed version.
`GET /api/v1/plugins/{id}/staged` returns that review again, so an update can be reviewed after the upload dialog has
closed. Nothing changes until you approve:

- The approval still has to name every capability the package asks for, not only the new ones. A body that leaves one
  out is refused with `409` and the staged package stays.
- MCP exposure is never carried over. Tick it again if you want it for the new version.
- The installed package, its approval, and its `plugins:` settings entry are kept as the single prior version in
  `<data dir>/plugins-prev/<id>/` (`package/` and `prev.json`). The directory is outside the plugins directory, so it is
  never loaded. An older prior version is replaced, so at most one is kept. `prev.json` can hold settings values, so it
  is readable by the server user only.
- The new package is then swapped in and activated: the plugin must load and one fresh instance's `init` must accept its
  settings. If that fails, the old directory, approval and prior version come back, the failed package is discarded, and
  the answer is `409 rolled back to <version>: <reason>`. A plugin you never configured, and a package without a wasm,
  are not checked beyond validation.

`POST /api/v1/plugins/{id}/rollback` (`SystemWrite`, not available as an MCP tool) swaps the installed package and the
prior one, with their approvals and settings entries, and runs the same activation check. The plugin list shows a
rollback button, with a confirmation, while a prior version exists. After a rollback the version you left becomes the
prior one, so a second rollback goes forward again. If the check fails, the rollback is undone and the answer is
`409`. A prior version that was never approved, was changed on disk, or no longer validates is refused.

If the server stops in the middle of an update, the installed package and its approval stay consistent, or the plugin
shows `pending_approval`. In that case approve the staged package again or roll back. Hidden `.swap~` leftovers are
removed by the next update of that plugin.

## Pre-publish checklist

- The manifest's `id`, `name`, `version`, `author` and `description` are your own. The template's values are
  placeholders. `bin/new-plugin.sh` fills in the `id` only.
- The `version` is a semver version of your plugin, and `api_version` is `1`, the host's ABI major version. The manifest
  can also set `min_tuliprox_version`.
- No secret is stored as a literal default. Every secret in `config-schema.json` has `writeOnly` or `format: password`,
  and no `default`. A secret is read from an environment reference, `${env:VAR}`, in `config.yml`. The upload is
  refused if a secret has a default.
- Your code never logs a secret, and never passes one on in a form the host cannot recognise.
- The manifest's `capabilities` lists only what the plugin uses. A capability that is not needed should be left out.
  The admin reads them at approval.
- Commit `Cargo.lock` and build with `--locked`. The template ships without a lock file (`cargo build` creates one the
  first time), so generate it and commit it. `bin/package-plugin.sh` adds `--locked` when a `Cargo.lock` exists. The
  examples under `plugins/examples/` each ship one.
- The SDK dependency in the template's `Cargo.toml` is a path to a tuliprox checkout. Before you share the crate outside
  that checkout, switch it to a published or git dependency.
- Compile every `ui/*.ts` to `ui/*.js`, and check that the frame documents load their scripts from the package.
- Do not commit `.wasm` or `.tlpx` files. Build them when you need them.
- Do not use the ids `metrics`, `staging` and `plugins-prev`.
- Run the unit tests with `cargo test` in the plugin directory. They run on the SDK's mock host.

## Template contents

`plugins/examples/template/.github/workflows/build.yml` is a CI workflow for a plugin repository. It installs the
`wasm32-wasip2` target, runs `cargo build --release --target wasm32-wasip2`, assembles the `.tlpx` with `zip`
(manifest, schema, `ui` without TypeScript sources, optional `rpc`, and `plugin.wasm`), and uploads it as an artifact.
Two things to change before it works for you: it builds without `--locked` (a comment in the file says to add it once
`Cargo.lock` is committed), and the SDK path dependency must be replaced by one that CI can reach.
