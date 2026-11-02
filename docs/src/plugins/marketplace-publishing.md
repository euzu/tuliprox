# Publishing to a Marketplace

A plugin repository is one JSON file, `marketplace.json`, that lists plugins and where to download their `.tlpx`
packages. An operator adds the URL of that file as a marketplace source in Tuliprox and can then browse and install
from it. This page is for publishers. Build the package first (see [Packaging and Publishing](packaging.md)).

## The file

```json
{
  "format_version": 1,
  "name": "Example plugins",
  "plugins": [
    {
      "id": "sports-enricher",
      "name": "Sports Enricher",
      "description": "Adds sports metadata to EPG entries.",
      "author": "Example Author",
      "license": "MIT",
      "category": "epg",
      "homepage": "https://github.com/example/plugins",
      "releases": [
        {
          "version": "1.2.0",
          "package_url": "https://github.com/example/plugins/releases/download/v1.2.0/sports-enricher-1.2.0.tlpx",
          "min_tuliprox_version": "3.3.0",
          "plugin_api": 1,
          "capabilities": ["network", "storage"],
          "changelog": "Handles double headers.",
          "published": "2026-11-01"
        }
      ]
    }
  ]
}
```

## Fields

Top level:

| Field | Required | Meaning |
| --- | --- | --- |
| `format_version` | yes | Must be `1`. Missing or `0` is invalid. A larger number fails the sync: "format_version N is newer than this Tuliprox understands (1)" |
| `name` | no | Display name of the repository |
| `plugins` | yes | The plugin list |

Each plugin:

| Field | Required | Meaning |
| --- | --- | --- |
| `id` | yes | The plugin id: `^[a-z][a-z0-9-]{2,40}$`, the same as the manifest `id`. Unique in the file. `staging`, `metrics`, `repositories` and `marketplace` are reserved |
| `name` | yes | Display name |
| `description`, `author`, `license`, `category`, `homepage` | no | Shown in the browser. `category` drives the category filter. `homepage` must be an `http(s)` URL |
| `releases` | yes | At least one valid release |

Each release:

| Field | Required | Meaning |
| --- | --- | --- |
| `version` | yes | Semver. Must equal the `version` in the package's `manifest.yml`. Unique within the plugin |
| `package_url` | yes | Direct `https://` URL of the `.tlpx`. No credentials in the URL |
| `plugin_api` | yes | The plugin API major the package is built for (`1` today) |
| `min_tuliprox_version` | no | Lowest Tuliprox version (semver). Absent means any |
| `capabilities` | no | Informational only |
| `changelog`, `published` | no | Shown with the release. `published` is `YYYY-MM-DD` |

Unknown fields are ignored at every level. Texts are cleaned of control characters and cut to a sensible length.

## What happens with mistakes

A problem in the whole file fails the sync and the operator keeps the previous catalog: invalid JSON, a bad
`format_version`, or two plugins with the same `id`. A problem in one plugin or release only skips that entry and shows
a warning, so one bad entry does not hide the rest. Skipped: an invalid id, a missing `name`, a version that is not
semver, a duplicate version, a missing `plugin_api`, a `package_url` that is not `https`.

## Compatibility

A release installs when `min_tuliprox_version` is at most the running Tuliprox version and `plugin_api` equals the
host's plugin API major. Otherwise it is still listed, marked incompatible with the reason, and cannot be installed.
Releases are shown newest first, compatible ones before incompatible ones.

## No checksums or signatures, by design

The format has no hash, checksum, signature or key field, and Tuliprox reads none. A checksum served from the same
place as the package proves nothing against someone who controls that place. Instead, the downloaded package is staged
and shown to the administrator as a normal review (capabilities, hooks, wasm hash). When the admin approves, Tuliprox
pins its own SHA-256 of the wasm and the approved capabilities. A package that later changes on disk stops running until
it is approved again. The `capabilities` in the catalog are a hint for browsing; what is approved is what the downloaded
package declares. The manifest id and version must match the catalog entry the operator chose, or the install is
rejected before anything is staged.

## Hosting

Any HTTPS file host works. The simplest setup is a GitHub repository:

1. Commit `marketplace.json` at the repository root. Operators use its raw URL, for example
   `https://raw.githubusercontent.com/<owner>/<repo>/main/marketplace.json`.
2. Attach each `.tlpx` to a GitHub release and put the release-asset URL in `package_url`
   (`https://github.com/<owner>/<repo>/releases/download/<tag>/<file>.tlpx`). Redirects, up to five, are followed as
   long as every hop stays on https.

Limits on the host side: the catalog is at most 2 MiB, a package at most 32 MiB, and the download must finish in 30
seconds (catalog) or 120 seconds (package). Package URLs that resolve to local or loopback addresses are refused.

## Building packages

Build each `.tlpx` with `bin/package-plugin.sh` (see [Packaging and Publishing](packaging.md)). Check that the
`manifest.yml` `id` and `version` equal the catalog entry.

## Validating the file

The JSON Schema of format 1 is `plugins/plugin-api/marketplace.schema.json` (JSON Schema 2020-12). Validate with any
tool, for example:

```sh
npx ajv-cli validate --spec=draft2020 -s plugins/plugin-api/marketplace.schema.json -d marketplace.json
```

The schema checks the shape; a duplicate id or version across entries is only caught by Tuliprox when it syncs.
