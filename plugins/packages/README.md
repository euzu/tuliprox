# Official plugin packages

These versioned `.tlpx` packages are the downloads listed in the root `marketplace.json`.
The catalog uses raw URLs on `euzu/tuliprox`'s `develop` branch. Publish the catalog
and its packages together; local edits do not update the hosted marketplace.

Packages include the eight plugins under `plugins/notifiers/` and the Matrix
notifier under `plugins/examples/matrix-notifier/`, plus YouTube Input under
`plugins/inputs/youtube-input/`. Matrix and YouTube Input are `0.1.0`; the other
notifiers are `1.0.0`. All require plugin API 1 and
Tuliprox 3.3.186 or newer.

To build the packages from source (requires the `wasm32-wasip2` Rust target):

```sh
for plugin_dir in plugins/notifiers/* plugins/examples/matrix-notifier plugins/inputs/youtube-input; do
    version=$(sed -n 's/^version: //p' "$plugin_dir/manifest.yml")
    id=$(basename "$plugin_dir")
    ./bin/package-plugin.sh "$plugin_dir" --out "plugins/packages/$id-$version.tlpx"
done
```

For a new release, bump its manifest and crate versions, build the new package,
and add its release entry to `marketplace.json`. Keep previous versioned packages
available for existing catalog releases.
