# TypeScript rename example

The TypeScript counterpart of `plugins/examples/template`: renames one group in `playlist.after-filter`. It is a
plain component built with ComponentizeJS, so it uses no SDK: settings come from `get-config()` and the rename
rule is a pure function in `src/rename.ts`.

**Verified** (jco 1.40, node 24): the component loads in the host, exports exactly `lifecycle` and
`playlist-hooks`, and renamed `News` to `Plugin News` in a served M3U, with one call and no failures. The unit
test in `test/rename.test.ts` (the pure rename rule, `npm test`) passes: 2 tests, 0 failures.

## Build

```sh
cd plugins/examples/typescript-rename
npm install
npm run build        # jco types, tsc check, esbuild bundle, jco componentize -> plugin.wasm (about 13 MB)
cd ../../..
bin/package-plugin.sh plugins/examples/typescript-rename --wasm plugins/examples/typescript-rename/plugin.wasm
# -> target/plugin-examples/typescript-rename.tlpx
```

Install it with the steps in `docs/src/plugins/quickstart.md` (upload, approve, settings
`{"rename_from": "News", "rename_to": "Plugin News"}`, enable, refresh). `npm test` runs the rename test.

## Things that differ from Rust

- A WIT `result` is a plain return for ok and a thrown value for err.
- The host import is `tuliprox:plugin/host@1.0.0`, with the version. `tsconfig.json` maps it to the generated
  types, and esbuild keeps it external (`--external:tuliprox:*`).
- Interface exports are camelCase objects: `lifecycle` and `playlistHooks`.
- The component embeds a JS engine (about 13 MB) and imports more WASI than a Rust one (`wasi:cli`,
  `wasi:filesystem`, `wasi:random`); the host provides them. A rename ran fine with `max_memory_mb: 32`; the
  manifest asks for 64 to leave headroom for large playlists.
