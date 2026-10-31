#!/usr/bin/env bash
#
# Builds the plugin test fixtures for wasm32-wasip2 and lays them out as a
# plugins directory, so the integration tests in backend/plugins (and a
# hand-run tuliprox) can load them:
#
#   target/plugin-fixtures/plugins/rename-group/{manifest.yml,plugin.wasm}
#   target/plugin-fixtures/plugins/<id>/{manifest.yml,plugin.wasm}
#       for every plugins/plugin-api/fixtures/host-probe/manifests/<id>.yml
#   target/plugin-fixtures/plugins/ui-demo/{manifest.yml,plugin.wasm,config-schema.json,
#       tab-schema.json,rpc/,ui/} and the same tree zipped as target/plugin-fixtures/ui-demo.tlpx
#
# host-probe is one component: each manifest grants different capabilities and
# resources, and every id gets its own copy of the same plugin.wasm.
#
# The components are never committed; CI does not build them. `target/` is
# already ignored. `make plugin-fixtures` runs this script.
#
# Usage:
#   bin/build_plugin_fixture.sh

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixtures="$root/plugins/plugin-api/fixtures"
out="$root/target/plugin-fixtures"
target="wasm32-wasip2"

if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    echo "build_plugin_fixture: missing rust target; run: rustup target add $target" >&2
    exit 1
fi

build() {
    cargo build --release --locked --target "$target" \
        --manifest-path "$fixtures/$1/Cargo.toml" \
        --target-dir "$out/build"
}

install_plugin() {
    local id="$1" wasm="$2" manifest="$3"
    local dest="$out/plugins/$id"
    mkdir -p "$dest"
    cp "$wasm" "$dest/plugin.wasm"
    cp "$manifest" "$dest/manifest.yml"
    echo "fixture plugin written to $dest"
}

build rename-group
build host-probe
build filter-probe
build epg-probe
build pipeline-probe
build event-probe
build notify-probe
build edge-probe
build provider-probe
build task-probe
build ui-demo

# Install one component once per manifest in <fixture>/manifests; each
# manifest's id must be its file name.
install_manifests() {
    local fixture="$1" wasm="$2" manifest id
    for manifest in "$fixtures/$fixture"/manifests/*.yml; do
        id="$(basename "$manifest" .yml)"
        if ! grep -qx "id: $id" "$manifest"; then
            echo "build_plugin_fixture: $manifest must declare 'id: $id' (the directory name)" >&2
            exit 1
        fi
        install_plugin "$id" "$wasm" "$manifest"
    done
}

# Each run starts from an empty plugins directory so no stale id survives.
rm -rf "$out/plugins"

install_plugin rename-group \
    "$out/build/$target/release/plugin_fixture_rename_group.wasm" \
    "$fixtures/rename-group/manifest.yml"
install_plugin filter-probe \
    "$out/build/$target/release/plugin_fixture_filter_probe.wasm" \
    "$fixtures/filter-probe/manifest.yml"
install_plugin provider-probe \
    "$out/build/$target/release/plugin_fixture_provider_probe.wasm" \
    "$fixtures/provider-probe/manifest.yml"

# ui-demo ships more than a manifest: schemas, rpc input schemas and the ui/
# frames. It is also packed as `ui-demo.tlpx` (manifest.yml at the zip root)
# for the upload route.
install_ui_demo() {
    local src="$fixtures/ui-demo" dest="$out/plugins/ui-demo" pack="$out/ui-demo.tlpx"
    install_plugin ui-demo "$out/build/$target/release/plugin_fixture_ui_demo.wasm" "$src/manifest.yml"
    cp "$src/config-schema.json" "$src/tab-schema.json" "$dest/"
    cp -R "$src/rpc" "$src/ui" "$dest/"
    rm -f "$pack"
    if command -v zip >/dev/null 2>&1; then
        (cd "$dest" && zip -q -r -X "$pack" .)
    else
        python3 -I -m zipfile -c "$pack" "$dest"/*
    fi
    echo "fixture package written to $pack"
}
install_ui_demo

install_manifests host-probe "$out/build/$target/release/plugin_fixture_host_probe.wasm"
install_manifests epg-probe "$out/build/$target/release/plugin_fixture_epg_probe.wasm"
install_manifests pipeline-probe "$out/build/$target/release/plugin_fixture_pipeline_probe.wasm"
install_manifests event-probe "$out/build/$target/release/plugin_fixture_event_probe.wasm"
install_manifests notify-probe "$out/build/$target/release/plugin_fixture_notify_probe.wasm"
install_manifests edge-probe "$out/build/$target/release/plugin_fixture_edge_probe.wasm"
install_manifests task-probe "$out/build/$target/release/plugin_fixture_task_probe.wasm"
