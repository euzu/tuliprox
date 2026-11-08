# Official plugin packages

These versioned `.tlpx` packages are the downloads listed in `plugins/marketplace.json`.
The catalog uses raw URLs on `euzu/tuliprox`'s `develop` branch. Publish the catalog
and its packages together; local edits do not update the hosted marketplace.

The package here is YouTube Input (`plugins/inputs/youtube-input/`), version `0.1.0`. It requires plugin API 1 and
Tuliprox 3.3.186 or newer. The notifier plugins are source examples under `plugins/examples/` and are not packaged here.

To build the packages from source (requires the `wasm32-wasip2` Rust target):

```sh
for plugin_dir in plugins/inputs/youtube-input; do
    version=$(sed -n 's/^version: //p' "$plugin_dir/manifest.yml")
    id=$(basename "$plugin_dir")
    ./bin/package-plugin.sh "$plugin_dir" --out "plugins/packages/$id-$version.tlpx"
done
```

For a new release, bump its manifest and crate versions, build the new package,
and add its release entry to `plugins/marketplace.json`. Keep previous versioned packages
available for existing catalog releases.
