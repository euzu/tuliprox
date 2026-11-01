# Settings, Secrets and Config Extensions

A plugin's settings are described by a JSON Schema file, and they are stored in `config.yml`, in the plugin's
`plugins.plugins[]` entry. The web UI shows them as a form, built from the schema, with no plugin JavaScript involved.
Inside the plugin, the SDK reads them with `#[tuliprox_plugin_settings]` (see the template) or `config::raw()`.

## The schema file

The manifest names the schema under `ui.config_section`:

```yaml
ui:
  config_section:
    schema: config-schema.json
```

This is the schema of the template, `plugins/examples/template/config-schema.json`:

```json
{
  "type": "object",
  "title": "My plugin",
  "properties": {
    "rename_from": {
      "type": "string",
      "title": "Group to rename",
      "description": "The exact playlist group name to match after filtering. Empty disables the rename.",
      "default": ""
    },
    "rename_to": {
      "type": "string",
      "title": "New group name",
      "default": ""
    },
    "api_token": {
      "type": "string",
      "title": "API token",
      "description": "A write-only secret: masked when read back, redacted from logs, no default.",
      "writeOnly": true
    }
  }
}
```

## Supported schema subset

The validator in `backend/plugins/src/settings.rs` accepts a subset of JSON Schema.

- Types: `string`, `number`, `integer`, `boolean`, `null`, `object`, `array`.
- Keywords: `enum`, `minimum`, `maximum`, `required`, `properties`, `items`, `additionalProperties`, plus annotations
  such as `title`, `description` and `default`.
- Not supported. A schema that uses these is refused: `oneOf`, `anyOf`, `allOf`, `$ref`, `if`, `then`, `else`, `not`,
  `dependentSchemas` and `patternProperties`.
- Nesting depth is limited to 32 levels.
- A schema or locale file larger than 1 MiB is refused (`backend/plugins/src/manifest/package.rs`).

A schema that uses an unsupported keyword is refused when the package is uploaded, and again when it is loaded.
Use `properties` for every field. Keep one flat object or a small nested one, as the SDK's
`#[tuliprox_plugin_settings]` struct expects.

## Saving and validation

`PUT /api/v1/plugins/{id}/settings` takes `{"settings": {...}}`. The save goes through these steps, in this order
(`PluginSystem::api_prepare_settings` in `backend/plugins/src/api.rs`, and
`backend/app/src/api/endpoints/plugins_settings_api.rs`):

1. A secret sent as the masked sentinel takes the stored value back. See [Secrets](#secrets).
2. The body is checked against the schema. A violation is refused with `422`, and the response lists each path in
   `fields`.
3. If the plugin is loaded, its `init` is run with the new settings in a throwaway instance. An error from `init` is
   refused with `422` and an `error` message. If the plugin is not loaded, `init` is skipped and the answer carries a
   warning that says so.
4. Only then is `config.yml` written, with a backup of the previous file, and the plugins are reloaded. The entry's
   `settings_version` is written as the schema's `version` (see [Versioning and migration](#versioning-and-migration)),
   which also clears a "needs review" notice.

The answer is `200` with the masked settings, the schema, the secret list and the `warnings`. A `503` means no
instance was free within the call deadline.

The saved values appear in `config.yml` under `plugins.plugins[]`, in that entry's `settings` map. The file is
rewritten from the parsed config, so comments in it are not kept.

A settings save for a plugin that has no entry creates the entry **disabled**. It does not enable the plugin, and it
does not approve it. See [Security Model](./security.md#trust-on-first-use).

## Versioning and migration

A settings schema may carry a top-level integer `version` (at least 1, default 1). Raise it whenever an update changes
the shape of the settings, such as a renamed field.

```json
{ "type": "object", "version": 2, "required": ["group"], "properties": { "group": { "type": "string" } } }
```

`version` sits next to `type` and `properties`. A property that happens to be named `version` is an ordinary setting.
A `version` that is not an integer of at least 1 makes the package invalid.

The stored settings remember the version they were written for, as `settings_version` in the `plugins.plugins[]`
entry. It is left out of `config.yml` when it is 1, and a missing value means 1. A settings save writes the schema's
current version.

When the stored version is lower than the installed schema's, one of these happens:

- **The plugin exports `settings.migrate`.** After the plugins reload, tuliprox calls it once, with the stored version
  and the stored settings. The settings it gets have their `${env:VAR}` references resolved, as `init` would see them.
  The version it returns is validated against the new schema. If it passes, it is saved to `config.yml` with the new
  `settings_version`, in the same write as a settings save, and the plugins reload. Resolved values are not written
  out: a string that is exactly a resolved `${env:VAR}` value goes back to the reference.
- **There is no `settings.migrate`, it returns `err`, or its result fails the schema.** The stored settings are left
  untouched, and the plugin list shows a "settings need review" notice with the reason, which the plugin's settings
  dialog repeats above the form. Saving the settings clears the notice.

If the old settings still pass the new schema, the plugin keeps running on them meanwhile. If they do not, the plugin
does not run: it is loaded, its state reads `failed` with the reason, and it runs again once a migration or a
save has written settings that fit.

When the stored version is higher than the schema's, as after a rollback to an older package, nothing is migrated down.
The notice is set, and the plugin runs only if the stored settings fit the older schema.

Only an enabled plugin is migrated. A disabled one is migrated after it is enabled. A migration that failed is not
retried until the package or the stored version changes.

A one-shot run without `-s` migrates the same way, but only in memory for that run: it never writes `config.yml`,
which a running server may share, and logs that the migration was not saved. The next server start saves it.

`migrate` is a function of the `settings-migration` interface, which the manifest lists as the hook `settings.migrate`.
It needs no capability. A plugin that exports it needs a tuliprox that has the interface, so its manifest should say
so with `min_tuliprox_version`. The interface is new in the 1.x line, so plugins built before it keep working.

```wit
interface settings-migration {
  migrate: func(from-version: u32, settings: string) -> result<string, string>;
}
```

`init` does not run before `migrate`: the old settings may not fit what the new `init` expects. Do not read the typed
`#[tuliprox_plugin_settings]` struct in `migrate`. Work on the JSON text instead.

With the SDK, implement `SettingsMigration` and let `Migrations` chain one step per old version. Each step turns the
settings of version `n` into those of `n + 1`; a gap in the chain, or a newer version than the last step leads to, is
an error.

```rust
use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin]
mod plugin {
    use super::*;

    impl SettingsMigration for MyPlugin {
        fn migrate(from_version: u32, settings: &str) -> Result<String, String> {
            Migrations::new()
                .step(1, |mut value| {
                    // 1 -> 2: `label` became `group`
                    if let Some(object) = value.as_object_mut() {
                        if let Some(label) = object.remove("label") {
                            object.insert("group".to_string(), label);
                        }
                    }
                    Ok(value)
                })
                .run(from_version, settings)
        }
    }
}
```

List `settings.migrate` under `hooks:` in the manifest. The fixture `plugins/plugin-api/fixtures/settings-migrate-probe`
is a complete example, from version 1 to 2.

<!-- UNVERIFIED: the web UI notice. The server side and the SDK are covered by unit and fixture tests; the dialog
rendering was not exercised in a browser. -->

## Secrets

A schema property is a secret when either of these holds:

- It has `"writeOnly": true`.
- It has `"format": "password"`.

A secret property must not have a `default`: a default ships inside the package, so everyone could read it. The
package is refused when one has. The template's `api_token` is the example: `writeOnly: true` and no `default`.

Source: `backend/plugins/src/manifest/secrets.rs`, and the test `a_secret_with_a_default_in_the_schema_is_invalid` in
`backend/plugins/src/registry.rs`.

Secret values follow these rules:

- **Storing.** A value can be a literal, or a `${env:VAR}` reference that is resolved when the config is loaded and when
  it is reloaded. Prefer environment references for every secret.
- **Reading back.** `GET /api/v1/plugins/{id}/settings` masks a literal secret with the sentinel `********`. It also
  returns `secrets`, which lists each secret path with `set: true` or `set: false`. A `${env:VAR}` reference is shown as
  the reference, not as the resolved value.
- **Saving back.** A `PUT` that sends the sentinel back keeps the stored value. The UI therefore never holds the secret.
- **Warnings.** A save that stores a literal secret returns a warning in `warnings`.

The sentinel is the constant `REDACTED_SECRET` in `shared/src/model/config/messaging.rs`. A value that mixes a
`${env:VAR}` reference with other text is masked, because the literal part may be the secret.

Inside the plugin, `get-config` returns the settings with `${env:VAR}` references resolved, so the plugin sees the real
value. Only that plugin's own instances see it. With the SDK, a `Secret<T>` field prints as `********` and gives the
plain value only through `expose()`.

The host replaces resolved secret values with `********` in the plugin's log lines, in task result text, and in the
result of an rpc call before it goes to the UI frame. It matches the exact text, so a secret the plugin has encoded or
cut is not recognised. The `extra` bag of a channel is built from the item's own fields, never from settings. See
[Security Model](./security.md#secrets).

## Config extensions

A plugin can add fields to a built-in config section with `ui.config_extensions`. Each entry has a `section` and an
inline JSON Schema fragment under `schema`. The section must be one of the web UI's config pages:

`main`, `api`, `log`, `schedules`, `messaging`, `webui`, `reverse_proxy`, `hdhomerun`, `proxy`, `ipcheck`, `mcp`,
`panel`, `recording`, `metadata_update`, `library`.

Any other section is refused when the package is uploaded or loaded. The values are stored in the plugin's settings
under `ext.<section>`, and a violation is reported at `ext.<section>.<field>`. The plugin's own schema must not declare
an `ext` property, and it never sees the `ext` values when it is validated. A secret in an extension fragment follows
the same rules, with the path `ext.<section>.<field>`.

None of the example plugins uses an extension. This is the spelling the manifest unit tests use
(`backend/plugins/src/manifest/package.rs`):

```yaml
ui:
  config_extensions:
    - section: proxy
      schema:
        type: object
        properties:
          key: {type: string, writeOnly: true}
```

The DTO is `PluginConfigExtensionDto` in `shared/src/model/plugins_dto.rs`.

<!-- UNVERIFIED: how the web UI renders an extension inside its section. The manifest side is checked in code and in
unit tests. The rendering is frontend behaviour that this page did not exercise in a browser. -->

## Config tabs

`ui.config_tab` adds a tab to the config page. The manifest fields are `title` or `title_key` (one is required),
`icon`, and exactly one of `schema` and `element` (`backend/plugins/src/manifest/validate.rs`).

- With `schema` the tab is a schema form, with no plugin JavaScript. This schema, like `ui.config_section`'s, is a
  settings schema: the first of the two is the one a settings save is checked against.
- With `element` the tab is a sandboxed frame, which counts as custom UI. The upload review then sets `customUi` to
  `true`. See [Custom UI](./custom-ui.md).

## Localisation of the form

Field titles and descriptions in the schema are plain text. Translated text for the plugin's own UI goes in
`ui/locales/<lang>.json`, which is described in [Custom UI](./custom-ui.md#localisation).
