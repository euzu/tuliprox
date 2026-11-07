# Python rename example

The Python counterpart of `plugins/examples/template`: renames one group in `playlist.after-filter`.

**Unverified.** Nothing here was built or run. The binding names are a guess until you check them against
your componentize-py version (see the header of `src/app.py`).

## Build (componentize-py)

```sh
pip install componentize-py
cd plugins/examples/python-rename
componentize-py -d ../../plugin-api/wit -w playlist-plugin componentize app -o plugin.wasm
# package like the Rust examples: a manifest, the wasm and the schema at the zip root
zip -j ../../../target/plugin-examples/python-rename.tlpx manifest.yml config-schema.json plugin.wasm
```

`bin/package-plugin.sh` builds with cargo, so it does not package this example; zip it as above.
Install it with the steps in `docs/src/plugins/quickstart.md`.
