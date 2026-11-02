# Plugin marketplace

The plugin marketplace lets you browse and install plugins from repository sources. A source is a plugin repository that
publishes a catalog. Tuliprox reads the catalog and downloads the packages it lists. It does not clone repositories.

This page covers how to add sources, how syncing and compatibility work, what an install does, and what you trust when you
add a source. To publish a catalog, see [Publishing a marketplace](./marketplace-publishing.md). For the rest of the security
model, see [Security Model](./security.md).

## Sources

A source is the direct HTTPS URL of a repository's root `marketplace.json`. For a GitHub repository, that is the raw file URL,
for example `https://raw.githubusercontent.com/<owner>/<repo>/main/marketplace.json`. The URL must point at the file itself,
not at the repository page.

You can add sources in two places. Both edit the same list in `config.yml`:

- In `config.yml`, under `plugins.repositories`.
- In the UI, in the Plugin Manager, on the **Marketplace** tab, under **Sources**.

```yaml
plugins:
  repositories:
    - id: example-plugins
      name: Example plugins
      url: https://raw.githubusercontent.com/example-org/tuliprox-plugins/main/marketplace.json
      enabled: true
    - id: private-plugins
      name: Private plugins
      url: ${env:PRIVATE_MARKETPLACE_URL}
      enabled: false
```

| Field | Rule |
| --- | --- |
| `id` | Unique among the sources. It has the same shape as a plugin id. |
| `name` | Required. Shown in the UI. |
| `url` | Must be `https`. It may be a `${env:VAR}` reference, which is resolved when the config is loaded. |
| `enabled` | A disabled source is not browsed and is not synced. |

When you edit a source in the UI, a `${env:VAR}` reference stays as you wrote it in `config.yml`. The UI shows the URL as
written, not the resolved value. A config reload picks up changes made in either place.

Removing a source deletes the source from `config.yml` and deletes its cached catalog. It does not uninstall or disable any
plugin that you installed from that source. Those plugins keep their approval and their data.

A disabled source keeps its entry and its cache, but its plugins are not listed in the Marketplace.

## Syncing

Tuliprox does not contact any source at startup. A source syncs in one of two cases:

- You press **Sync** for that source.
- You open the Marketplace while that source's cache is empty. This happens once per source per start.

Each source shows the time of its last sync and its last error, if the last sync failed. A failed sync keeps the previous
catalog, so the plugins you saw before remain listed. Fix the problem, then press **Sync** again.

A catalog may be at most 2 MiB. A larger catalog fails the sync.

A catalog-level problem fails the whole sync. Examples are invalid JSON, a duplicate plugin id, or a `format_version` this
Tuliprox does not understand. An invalid plugin or release is skipped, and the sync result lists it as a warning. The other
entries still appear.

## Browsing and compatibility

The Marketplace lists plugins from all enabled sources. You can filter them by:

- Search text.
- Category.
- Source.

Each release has a compatibility state:

| Check | Required |
| --- | --- |
| Minimum Tuliprox version | At most the running Tuliprox version. A release with no minimum version has no requirement. |
| Plugin API | Equal to the host's plugin API major, which is `1`. |

A release that passes both checks is installable. A release that fails either check is still listed, but its install action is
disabled, and the UI shows the reason. Releases are sorted newest first, with installable releases before incompatible ones.

A plugin offered by several sources appears once per source. Each entry shows the source it comes from. You choose which
source to install from. Tuliprox never picks a source for you.

## Installing

Installing is a review and an approval, the same as uploading a `.tlpx` package. The steps are:

1. You choose a plugin, a version and a source, then press **Install**.
2. Tuliprox downloads the package from the URL in the catalog. The package may be at most 32 MiB. Only `https` is accepted,
   for the package URL and for every redirect. Destinations local to the host are refused: loopback,
   link-local (including cloud metadata endpoints), unspecified, multicast and broadcast addresses.
3. Tuliprox validates the package and reads its manifest. The manifest's id and version must match the release you chose.
   If they do not, the install fails.
4. The package goes through the same validation and capability review as an uploaded `.tlpx`. The review lists the
   capabilities, the hooks, the secret settings paths and the other items described in [Security Model](./security.md).
5. You approve the capabilities. At approval, Tuliprox computes the SHA-256 of `plugin.wasm` itself and pins it, together
   with the capability names. Tuliprox does not use a hash from the catalog.
6. You enable the plugin afterwards.

Nothing is approved or enabled automatically. A downloaded package that stays unapproved does nothing.

After approval, the installed plugin shows **Installed from `<source>` @ `<version>`**. This origin is stored with the approval
pin, so it survives restarts and rollbacks.

If the package is invalid or incompatible, the install fails and the existing installation is left as it was. This includes
an install of a version whose manifest id or version does not match the release.

Installing an update to a plugin that is already installed uses the same staged update and rollback flow as a manual update.
The review shows the plugin it replaces and the capability changes. The previous version is kept so you can roll back.

## Updates

An installed marketplace plugin can be updated from the source it was installed from. An update never comes from another
source that lists the same plugin id. Checks run on demand only. Tuliprox has no update schedule.

### Checking for updates

Press **Check for updates** in the Plugin Manager. It syncs only the sources of installed marketplace plugins, not every
source you have configured. A plugin whose source is removed or disabled gets no update.

After a check, each installed plugin with a newer compatible release shows one of two badges:

| Badge | Meaning |
| --- | --- |
| **Eligible** | The update has no blocker. You can apply it with **Update** or **Update all eligible**. |
| **Awaiting approval** | The update needs your review. The plugin lists the reasons. |

An update is awaiting approval when any of these is true:

- It adds a capability that the installed version does not have.
- It exposes more MCP tools than the installed version.
- Its settings schema changed and there is no working settings migration.
- Its settings schema version is lower than the installed one.

Each update also shows how it was checked:

- **verified**: Tuliprox downloaded the package and checked it.
- **from catalog**: the decision is based on the catalog entry only. The package has not been downloaded yet.

### Applying updates

- **Update** applies one update. It works under any update policy.
- **Update all eligible** applies every eligible update, except for plugins whose policy is `pinned`.

An update that is awaiting approval opens the usual review dialog, with the capability differences from the installed
version. Approving it activates the update. Tuliprox computes the SHA-256 of the new `plugin.wasm` locally and pins that
hash. It does not use a hash from the catalog.

### Update policies

Each plugin has an update policy. The default is `auto`.

| Policy | At boot | With **Update all eligible** |
| --- | --- | --- |
| `auto` | Applied when boot updates are on and the update is eligible. | Applied when eligible. |
| `manual` | Never applied. The update stays staged. | Applied when eligible. |
| `pinned` | Never applied. The update stays staged. | Never applied. |

A single **Update** works under every policy. Use `pinned` to keep a plugin out of boot updates and bulk updates. You can
still update it by hand.

Boot updates and the policy are set in `config.yml`:

```yaml
plugins:
  auto_update_on_boot: true
  boot_update_budget: 30s
  plugins:
    - id: example-plugin
      update_policy: pinned
```

### Boot updates

Boot updates are off by default. Set `auto_update_on_boot: true` to turn them on.

Plugins start on their current version first, so startup never waits for an update. After the plugins load, a background run
checks the sources and applies eligible updates. The run stops when `boot_update_budget` runs out. The default budget is `30s`.

A boot update applies only when all of these are true:

- The plugin was installed from a marketplace.
- The plugin is enabled.
- The plugin's policy is `auto`.
- The update has no blocker.

Anything else is staged as **Awaiting approval**, and the current version keeps running. Boot updates never install a plugin
and never enable one.

A failure, an unreachable source or an exhausted budget keeps the current version. The next boot tries again. Tuliprox does not
retry within the same run. The outcome of the last boot run is shown as **Last boot update report** in the Plugin Manager.

### Safety

- **Activation is journaled.** Tuliprox records each update before it changes any files. If Tuliprox stops in the middle of an
  update, the next start restores the previous version before any plugin loads. The restore is logged and shown in the boot report.
- **A failed init rolls back.** An update whose `init` fails is rolled back automatically, and the previous version keeps running.
- **Running tasks delay activation.** Running plugin tasks delay a boot activation, up to the remaining budget. If tasks are still
  running when the budget ends, that plugin is skipped until the next boot.

### Events

Updates emit three events. Route them to notification channels to be told about them:

| Event | When |
| --- | --- |
| `plugin.update.available` | The boot run staged an update that is awaiting approval. |
| `plugin.update.applied` | An update was activated. |
| `plugin.update.failed` | An update failed. |

Each event has these payload fields: `outcome`, `plugin`, `from`, `to`, `source` and `reason`.

## Trust model

Read this section before you add a source.

- **You trust the sources you add.** A source decides which packages are offered to you. Add only sources you trust.
- **Catalogs carry no checksums, signatures or keys.** This is by design. Tuliprox does not check any signature, and it does
  not read any publisher key or package hash from a catalog.
- **The local pin detects later changes on disk.** It shows that a package changed after you approved it. It does not prove
  who published the package.
- **The admin approval is the security boundary.** You approve the capabilities and the package you reviewed. Nothing from
  the catalog bypasses that step.

Review the capabilities before you approve. Pay particular attention to `network`. A plugin with `network` may call any host
on your network, including services that are not meant to be public. See [Network](./security.md#network).

## API

The Marketplace is available through the management API under `/api/v1`. Reads need the `SystemRead` permission and changes need
`SystemWrite`. These routes are not MCP tools. They return `404` when the plugin system is off.

| Method and path | Purpose |
| --- | --- |
| `GET /plugins/repositories` | List the configured sources, with their sync state. |
| `POST /plugins/repositories` | Add a source. Returns `422` on a validation error and `409` for a duplicate id. |
| `PUT /plugins/repositories/{id}` | Replace a source. The id in the body must match the path. |
| `DELETE /plugins/repositories/{id}` | Remove a source and its cache. Installed plugins are not removed. |
| `POST /plugins/repositories/{id}/sync` | Sync a source now. Returns `502` with the error when the sync fails. |
| `GET /plugins/marketplace?q=&category=&source=` | List plugins from enabled sources, with the filters applied. |
| `POST /plugins/marketplace/install` | Install one release from one named source. Returns the same review as an upload. |
