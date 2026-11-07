#!/usr/bin/env bash
#
# Packages a plugin directory into a .tlpx archive for the upload route
# (POST /api/v1/plugins). Builds plugin.wasm first unless --wasm is given.
#
# Usage:
#   bin/package-plugin.sh <plugin-dir> [--wasm FILE] [--out FILE.tlpx]
#
# The archive is laid out the way backend/plugins/src/install.rs expects it:
#   manifest.yml                 at the ZIP ROOT, always
#   plugin.wasm                  iff the manifest lists hooks (built, or --wasm)
#   config-schema.json           iff the file exists
#   every file the manifest names: schema and element paths, locale files
#   ui/**                        iff ui/ exists (every regular file, except TypeScript
#                                sources: *.ts, *.tsx, *.tsbuildinfo, tsconfig*.json;
#                                the compiled .js ships instead)
#   rpc/*.json                   iff rpc/ exists
# Entries are sorted, have no directory entries, no symlinks and no nested top
# directory. Default output: target/plugin-examples/<id>.tlpx under the repo.
#
# A referenced file that is missing or is a symlink fails the run. The manifest
# is read with a small line-based scanner (stdlib python3 only, no YAML library),
# so it handles the manifest shapes the template and the fixtures use: top-level
# `id:` and `hooks:`, `locales:` as a flow or block list, and scalar `schema:`,
# `element:` and `input_schema:` paths.
#
# Build: cargo with the wasm32-wasip2 target. Needs python3 (3.8 or later).

set -euo pipefail

usage() {
    echo "usage: bin/package-plugin.sh <plugin-dir> [--wasm FILE] [--out FILE.tlpx]" >&2
    exit 2
}

die() {
    echo "package-plugin: $*" >&2
    exit 1
}

# Reads the manifest and prints id=, crate=, hooks= (1 or 0). Writes nothing.
# (The embedded Python is read with `read -d ''`, not `$(cat <<PY)`, which bash 3.2 mis-parses.)
read -r -d '' scan_py <<'PY' || true
import os, re, sys

ID_RE = re.compile(r"^[a-z][a-z0-9-]{2,40}$")
REF_KEYS = ("schema", "element", "input_schema")
KEY_RE = re.compile(r"^(\s*)([A-Za-z_][A-Za-z0-9_-]*):(.*)$")


def fail(message):
    sys.exit("package-plugin: " + message)


def indent_of(line):
    return len(line) - len(line.lstrip(" "))


def unquote(value):
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def bare(value):
    # Drops a trailing comment (" #"). Manifest values in this repo never hold " #".
    return value.split(" #", 1)[0].strip()


def list_after(lines, index, key):
    indent = indent_of(lines[index])
    inline = bare(lines[index].split(":", 1)[1])
    if inline.startswith("["):
        if not inline.endswith("]"):
            fail("`%s` must be a flow list on one line" % key)
        return [unquote(part) for part in inline[1:-1].split(",") if part.strip()]
    if inline:
        fail("`%s` must be a list" % key)
    items = []
    for line in lines[index + 1:]:
        text = line.strip()
        if not text or text.startswith("#"):
            continue
        current = indent_of(line)
        if current < indent or (current == indent and not text.startswith("- ")):
            break
        if not text.startswith("- "):
            fail("unsupported line under `%s`: %s" % (key, text))
        items.append(unquote(bare(text[2:])))
    return items


def read_manifest(path):
    if not os.path.isfile(path):
        fail("no manifest.yml in " + os.path.dirname(path))
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    manifest_id = None
    hooks = []
    locales = []
    refs = []
    for index, line in enumerate(lines):
        match = KEY_RE.match(line)
        if not match:
            continue
        indent = len(match.group(1))
        key = match.group(2)
        value = bare(match.group(3))
        if indent == 0 and key == "id":
            manifest_id = unquote(value)
        elif indent == 0 and key == "hooks":
            hooks = list_after(lines, index, key)
        elif key == "locales":
            locales = list_after(lines, index, key)
        elif key in REF_KEYS and value and value[0] not in "[{":
            refs.append(unquote(value))
    return manifest_id, hooks, locales, refs


def scan(root):
    manifest_id, hooks, _, _ = read_manifest(os.path.join(root, "manifest.yml"))
    if not manifest_id or not ID_RE.match(manifest_id):
        fail("manifest id `%s` must match ^[a-z][a-z0-9-]{2,40}$" % (manifest_id or ""))
    crate = ""
    cargo = os.path.join(root, "Cargo.toml")
    if os.path.isfile(cargo):
        with open(cargo, encoding="utf-8") as handle:
            text = handle.read()
        section = re.search(r"^\[package\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
        name = re.search(r'^name\s*=\s*"([^"]+)"', section.group(1), re.M) if section else None
        if name:
            crate = name.group(1).replace("-", "_")
    print("id=" + manifest_id)
    print("crate=" + crate)
    print("hooks=" + ("1" if hooks else "0"))


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] != "scan":
        fail("internal: scan mode takes <plugin-dir>")
    scan(os.path.abspath(sys.argv[2]))
PY

# Collects the package entries, checks them, and writes the zip.
read -r -d '' pack_py <<'PY' || true
import os, re, stat, sys, zipfile

ID_RE = re.compile(r"^[a-z][a-z0-9-]{2,40}$")
NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")
REF_KEYS = ("schema", "element", "input_schema")
KEY_RE = re.compile(r"^(\s*)([A-Za-z_][A-Za-z0-9_-]*):(.*)$")


def fail(message):
    sys.exit("package-plugin: " + message)


def indent_of(line):
    return len(line) - len(line.lstrip(" "))


def unquote(value):
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def bare(value):
    return value.split(" #", 1)[0].strip()


def list_after(lines, index, key):
    indent = indent_of(lines[index])
    inline = bare(lines[index].split(":", 1)[1])
    if inline.startswith("["):
        if not inline.endswith("]"):
            fail("`%s` must be a flow list on one line" % key)
        return [unquote(part) for part in inline[1:-1].split(",") if part.strip()]
    if inline:
        fail("`%s` must be a list" % key)
    items = []
    for line in lines[index + 1:]:
        text = line.strip()
        if not text or text.startswith("#"):
            continue
        current = indent_of(line)
        if current < indent or (current == indent and not text.startswith("- ")):
            break
        if not text.startswith("- "):
            fail("unsupported line under `%s`: %s" % (key, text))
        items.append(unquote(bare(text[2:])))
    return items


def read_manifest(path):
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    manifest_id = None
    hooks = []
    locales = []
    refs = []
    for index, line in enumerate(lines):
        match = KEY_RE.match(line)
        if not match:
            continue
        indent = len(match.group(1))
        key = match.group(2)
        value = bare(match.group(3))
        if indent == 0 and key == "id":
            manifest_id = unquote(value)
        elif indent == 0 and key == "hooks":
            hooks = list_after(lines, index, key)
        elif key == "locales":
            locales = list_after(lines, index, key)
        elif key in REF_KEYS and value and value[0] not in "[{":
            refs.append(unquote(value))
    return manifest_id, hooks, locales, refs


def regular_file(root, rel, what):
    path = os.path.join(root, rel)
    if os.path.islink(path):
        fail("%s `%s` is a symlink; packages carry regular files only" % (what, rel))
    if not os.path.isfile(path):
        fail("%s `%s` is missing from %s" % (what, rel, root))
    return path


def check_relative(ref):
    parts = ref.split("/")
    if not ref or ref.startswith("/") or "\\" in ref or any(part in ("", ".", "..") for part in parts):
        fail("manifest path `%s` must be relative, inside the plugin directory" % ref)


def tree_files(root, rel):
    base = os.path.join(root, rel)
    if os.path.islink(base):
        fail("`%s` is a symlink; packages carry regular files only" % rel)
    if not os.path.isdir(base):
        fail("`%s` is not a directory" % rel)
    found = {}
    for current, dirs, names in os.walk(base):
        for name in sorted(dirs) + sorted(names):
            path = os.path.join(current, name)
            if os.path.islink(path):
                fail("`%s` is a symlink; packages carry regular files only" % os.path.relpath(path, root))
        for name in names:
            if is_typescript_source(name):
                continue
            path = os.path.join(current, name)
            found[os.path.relpath(path, root).replace(os.sep, "/")] = path
    return found


def is_typescript_source(name):
    # Sources and build leftovers stay in the repo; the compiled .js is what ships.
    return name.endswith((".ts", ".tsx", ".tsbuildinfo")) or (name.startswith("tsconfig") and name.endswith(".json"))


def collect(root, wasm):
    manifest_id, hooks, locales, refs = read_manifest(os.path.join(root, "manifest.yml"))
    if not manifest_id or not ID_RE.match(manifest_id):
        fail("manifest id `%s` must match ^[a-z][a-z0-9-]{2,40}$" % (manifest_id or ""))
    files = {"manifest.yml": regular_file(root, "manifest.yml", "manifest")}
    if hooks:
        if not wasm:
            fail("the manifest lists hooks, so plugin.wasm is required")
        if os.path.islink(wasm) or not os.path.isfile(wasm):
            fail("plugin.wasm `%s` is not a regular file" % wasm)
        files["plugin.wasm"] = os.path.abspath(wasm)
    elif wasm:
        fail("the manifest lists no hooks, so plugin.wasm must not be packaged")
    if os.path.lexists(os.path.join(root, "config-schema.json")):
        files["config-schema.json"] = regular_file(root, "config-schema.json", "config-schema.json")
    for locale in locales:
        if not NAME_RE.match(locale):
            fail("locale `%s` is not a plain language code" % locale)
        rel = "ui/locales/%s.json" % locale
        files[rel] = regular_file(root, rel, "locale file")
    for ref in refs:
        check_relative(ref)
        files[ref] = regular_file(root, ref, "file named by the manifest")
    if os.path.lexists(os.path.join(root, "ui")):
        files.update(tree_files(root, "ui"))
    if os.path.lexists(os.path.join(root, "rpc")):
        rpc_dir = os.path.join(root, "rpc")
        if os.path.islink(rpc_dir) or not os.path.isdir(rpc_dir):
            fail("`rpc` must be a real directory")
        for name in sorted(os.listdir(rpc_dir)):
            if name.endswith(".json"):
                rel = "rpc/" + name
                files[rel] = regular_file(root, rel, "rpc schema")
    return files


def pack(root, wasm, out):
    files = collect(root, wasm)
    out = os.path.abspath(out)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    partial = out + ".partial"
    try:
        with zipfile.ZipFile(partial, "w", zipfile.ZIP_DEFLATED) as archive:
            for arc in sorted(files):
                info = zipfile.ZipInfo(arc, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (stat.S_IFREG | 0o644) << 16
                with open(files[arc], "rb") as handle:
                    archive.writestr(info, handle.read())
        os.replace(partial, out)
    except BaseException:
        if os.path.exists(partial):
            os.remove(partial)
        raise
    print(out)


if __name__ == "__main__":
    if len(sys.argv) != 5 or sys.argv[1] != "pack":
        fail("internal: pack mode takes <plugin-dir> <wasm-or-empty> <out>")
    pack(os.path.abspath(sys.argv[2]), sys.argv[3] or None, sys.argv[4])
PY

[[ $# -ge 1 ]] || usage
dir=""
wasm=""
out=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --wasm)
            [[ $# -ge 2 ]] || die "--wasm needs a file"
            wasm="$2"
            shift 2
            ;;
        --out)
            [[ $# -ge 2 ]] || die "--out needs a file"
            out="$2"
            shift 2
            ;;
        -h | --help)
            usage
            ;;
        -*)
            die "unknown option $1"
            ;;
        *)
            [[ -z "$dir" ]] || die "only one plugin directory can be given"
            dir="$1"
            shift
            ;;
    esac
done
[[ -n "$dir" ]] || usage
[[ -d "$dir" ]] || die "$dir is not a directory"
dir="$(cd "$dir" && pwd)"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="$root/target/plugin-examples/build"

scan_out="$(python3 -I -c "$scan_py" scan "$dir")"
id="$(printf '%s\n' "$scan_out" | sed -n 's/^id=//p')"
crate="$(printf '%s\n' "$scan_out" | sed -n 's/^crate=//p')"
hooks="$(printf '%s\n' "$scan_out" | sed -n 's/^hooks=//p')"

if [[ "$hooks" == 1 && -z "$wasm" ]]; then
    [[ -f "$dir/Cargo.toml" ]] || die "the manifest lists hooks, but $dir has no Cargo.toml to build plugin.wasm from; pass --wasm FILE"
    [[ -n "$crate" ]] || die "cannot read the [package] name from $dir/Cargo.toml"
    build_args=(cargo build --release --target wasm32-wasip2 --manifest-path "$dir/Cargo.toml" --target-dir "$target_dir")
    if [[ -f "$dir/Cargo.lock" ]]; then
        build_args+=(--locked)
    fi
    echo "package-plugin: building $id for wasm32-wasip2" >&2
    "${build_args[@]}"
    wasm="$target_dir/wasm32-wasip2/release/$crate.wasm"
fi

if [[ -z "$out" ]]; then
    out="$root/target/plugin-examples/$id.tlpx"
fi

python3 -I -c "$pack_py" pack "$dir" "$wasm" "$out"
