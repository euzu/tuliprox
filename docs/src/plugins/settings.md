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

The validator in `shared/src/utils/plugin_schema.rs` accepts a subset of JSON Schema. The server checks settings and
rpc payloads with it, and the source editor checks the `plugin_config` of an input or output with the same code
before it applies the form, so both report the same violations.

- Types: `string`, `number`, `integer`, `boolean`, `null`, `object`, `array`. A list of one type and `null`
  (`"type": ["string", "null"]`) is a field that may also be `null`.
- Keywords: `enum`, `const`, `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`,
  `minLength`, `maxLength`, `pattern`, `format`, `required`, `properties`, `items`, `minItems`, `maxItems`,
  `uniqueItems`, `additionalProperties`, plus annotations such as `title`, `description` and `default`.
- `pattern` is a regular expression in the syntax of the Rust `regex` crate (no look-around, no back-references) and,
  as in JSON Schema, matches anywhere in the text unless it is anchored with `^…$`. A pattern that does not compile is
  refused when the package is checked. `x-pattern-message` replaces the default "must match the pattern" message.
- `format` is checked for `email`, `uri` (a scheme and a rest, like `https://…`), `hostname`, `date-time`
  (RFC 3339, like `2026-01-31T18:00:00Z`), `date`, `time` and `duration` (ISO 8601, like `PT30M`). Any other
  format is an annotation.
- Not supported. A schema that uses these is refused: `oneOf`, `anyOf`, `allOf`, `$ref`, `if`, `then`, `else`, `not`,
  `dependentSchemas` and `patternProperties`.
- Nesting depth is limited to 32 levels.
- A schema or locale file larger than 1 MiB is refused (`backend/plugins/src/manifest/package.rs`).

A schema that uses an unsupported keyword is refused when the package is uploaded, and again when it is loaded.
Use `properties` for every field. Keep one flat object or a small nested one, as the SDK's
`#[tuliprox_plugin_settings]` struct expects.

### How the form renders a schema

`SchemaForm` (`frontend/src/app/components/plugins/schema_form.rs`) renders the plugin's settings and its input and
output forms. Fields appear in the order the schema lists them. A string is a text box (`format: email`, `uri` and
`date` use the browser's matching input), an integer or number a number box, a boolean a switch, an `enum` a select,
an array of scalars a tag list, an array whose `items` is an `enum` a multi-select, and an object a collapsible group,
which may hold further groups. An array of objects or of arrays is not rendered.

A schema can add UI hints. They start with `x-`, so the validator ignores them, and they never change what is valid:

| Hint | On | Effect |
| --- | --- | --- |
| `x-widget: textarea` / `code` | string | A multi-line box; `code` uses a monospace font. Both take a row of their own. |
| `x-widget: color` | string | A colour picker storing `#rrggbb`. |
| `x-widget: slider` | integer, number with `minimum` and `maximum` | A slider over the range, stepping by `multipleOf`. |
| `x-widget: radio` | `enum` | Radio buttons instead of a select. |
| `x-widget: duration` | integer, string | An amount and a unit (seconds to days). An integer stores seconds, a string `30m`, or `PT30M` with `format: duration`. |
| `x-enum-labels` | `enum`, or an array with `items.enum` | Labels for the values: a list in the order of `enum`, or an object from value to label. |
| `x-placeholder` | any field | The text shown while the field is empty. |
| `x-help-url` | any field | An `http(s)` link next to the label; any other URL is dropped. |
| `x-columns: 1`–`4` | the root object, a group | Lays its fields out in that many columns (one on narrow screens). |
| `x-row: true` | any field | The field takes a row of its own in a multi-column layout. |
| `x-collapsed: true` | a group | The group starts collapsed. |
| `x-pattern-message` | string with `pattern` | The error shown when the pattern does not match. |

A widget that does not fit the field's type is ignored, and so is a slider without both bounds.

```json
{
  "type": "object",
  "x-columns": 2,
  "properties": {
    "mode": { "enum": ["live_only", "all"], "x-enum-labels": ["Live streams only", "Everything"], "x-widget": "radio" },
    "refresh": { "type": "integer", "minimum": 60, "default": 3600, "x-widget": "duration" },
    "notes": { "type": "string", "x-widget": "textarea", "x-placeholder": "Anything the next admin should know" },
    "slug": { "type": "string", "pattern": "^[a-z0-9-]+$", "x-pattern-message": "lowercase letters, digits and dashes" }
  }
}
```

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

## Input and output forms

A plugin that provides a `type: plugin` input or output can describe the fields its own instances need with
`ui.input_form` and `ui.output_form`.

```yaml
ui:
  input_form:
    - id: main            # shown inside the host's Main panel
      schema: forms/input.json
    - id: options         # shown inside the host's Options panel
      schema: forms/options.json
    - id: filters         # a panel of its own, titled by the plugin
      title_key: form.filters
      schema: forms/filters.json
  output_form:
    - id: main
      schema: forms/output.json
```

- Each entry has an `id`, a package-relative `schema`, and optionally a `title` or `title_key`. Ids are kebab case and
  unique per form.
- `icon` names an icon of the web UI's icon set for the panel's sidebar entry (the default is `Options`; the Main
  panel keeps the host's icon). `description` or `description_key` is shown above the section's fields.
- `actions` adds buttons under the section's fields, see [Form actions](#form-actions).
- The schema is the same draft 2020-12 subset `SchemaForm` renders and the settings schemas use: an object schema,
  no `oneOf`/`anyOf`/`allOf`/`$ref`/conditional keywords, the UI hints of
  [How the form renders a schema](#how-the-form-renders-a-schema), and a `title`/`description` that is a locale key
  is translated.
- When the user applies the form, every value they left alone that has a `default` is filled in, and the whole
  `plugin_config` is checked against all of the form's sections together. A violation keeps the form open, switches
  to the panel of the first one and shows each message under its field; the messages then follow the user's edits.
- `main` and `options` extend the host's own panels. The host always shows the fields every input or output has:
  for an input the plugin, name, enabled, sequential group, max connections, priority, cache duration, url and playback
  provider; for an output the plugin, file name and filter. Any other id adds a panel after them.
- A form field must not be a secret (`writeOnly`/`format: password` is refused): the values are stored in `source.yml`,
  which the host does not mask. Keep credentials in the plugin's settings.

The values are stored per input or output, in `plugin_config`:

```yaml
inputs:
  - name: youtube-news
    type: plugin
    plugin: youtube-input
    url: "https://www.youtube.com/@tagesschau"
    plugin_config:
      mode: live_only
      max_uploads: 5
```

The host hands them to the plugin as `input-request.options` / `output-request.options`, which the ABI already
carries. A string value keeps its text, any other value is its JSON text (`true`, `10`, `["a"]`), and a nested object
is flattened with dotted keys, so `auth.user` arrives as one option named `auth.user`. The plugin reads the keys it
declares; unknown keys are the plugin's to ignore.

A list therefore arrives as its JSON text, not as a repeated option: `["https://a","https://b"]` is one option named
after the property. A plugin that declares an array field parses it (`serde_json::from_str::<Vec<String>>`), so it
should accept a plain comma-separated string as well — hand-written YAML often uses one.

The usual split: values that describe the deployment belong in the plugin's settings, values that describe one input or
output in its form. A plugin whose whole configuration is per-instance declares no settings schema at all — then the
Plugin Manager's settings dialog says so and points at the input form, and `GET …/settings` returns `schema: null`.
`youtube-input` works this way: every field it needs is on each input.

Declaring a field both as a setting and in the form is supported: the setting is the default, an empty field keeps it,
and the plugin writes that fallback. Host-side settings are never plugin settings — the update policy and the boot
update budget live in `plugins:`, not in a package.

## Source editor tiles

Every enabled input provider (`input.fetch`) gets its own tile in the **Inputs** group of the source editor sidebar,
and every enabled output renderer (`output.render`) one in the **Output** group. Dropping the tile, or tapping it on a
phone, adds a `type: plugin` input or output that already names the plugin, so its form opens with the plugin's
fields. The plugin can still be changed in the form's **Plugin** field.

The tile works without any manifest entry: it is titled with the plugin's `name` and its tooltip is the plugin's
`description`. `ui.input_tile` and `ui.output_tile` change that:

```yaml
ui:
  locales: [en, de]
  input_tile:
    title: YouTube              # at most 40 characters; default: the plugin's name
    title_key: tile.title       # translated title, from ui/locales
    icon: PlayCircle            # an icon name of the web UI's icon set
    description: Channels, playlists and live streams   # tooltip; default: the plugin's description
    description_key: tile.description
```

- Every field is optional. `title` must not contain control characters. The tooltip is cut at 512 characters.
- `title_key` and `description_key` need `ui.locales`, like every other key.
- `ui.input_tile` needs the hook `input.fetch` and `ui.output_tile` needs `output.render`; a manifest that declares one
  without its hook is refused.
- `icon` names an icon the web UI already has (for example `PlayCircle`, `Live`, `Movie`, `Playlist`, `Streams`). An
  unknown name shows no icon; it is never loaded as an image.
- A disabled or failed provider has no tile. When the list of plugins is loaded and no provider is installed, the
  group shows a greyed-out **Plugin** tile with a note instead. Until the list is loaded, and for a user who may not
  read the system, the generic **Plugin** tile stays, with the plugin chosen in the form.

On the canvas a plugin block shows the plugin's tile title where other blocks show their type. A block whose plugin
cannot run gets a red badge and a dashed border, and its tooltip says why: no plugin chosen, the plugin is not
installed (or not loaded), it is disabled or failed, or it no longer provides inputs or outputs. The block is kept as
it is, and saving still goes through the server's own check.

`GET /api/v1/plugins/ui` lists every input and output provider, with or without a `ui:` part, and sends the resolved
tile as `inputTile` / `outputTile` (`PluginSourceTileDto` in `shared/src/model/plugins_dto.rs`).

This is not the `source-editor-tile` [contribution slot](./custom-ui.md#slots): that one is a sandboxed frame on the
canvas, shared by every contributing plugin, and is not an input or output.

`plugin_config` is refused on any other input type, and a plugin input still rejects the built-in downloader fields
(aliases, `media_server`, `panel_api`, `stalker`, provider failover definitions).

## Config tabs

`ui.config_tab` adds a tab to the config page. The manifest fields are `title` or `title_key` (one is required),
`icon`, and exactly one of `schema` and `element` (`backend/plugins/src/manifest/validate.rs`).

- With `schema` the tab is a schema form, with no plugin JavaScript. This schema, like `ui.config_section`'s, is a
  settings schema: the first of the two is the one a settings save is checked against.
- With `element` the tab is a sandboxed frame, which counts as custom UI. The upload review then sets `customUi` to
  `true`. See [Custom UI](./custom-ui.md).

### Form actions

A section can carry buttons that call one of the plugin's [rpc exports](./custom-ui.md#rpc_exports) with the values the form holds
now, before they are saved, for example to test a connection or to preview what an input would fetch.

```yaml
hooks: [input.fetch, rpc.call]
ui:
  rpc_exports:
    - { name: check-channel, kind: read }
  input_form:
    - id: main
      schema: forms/input.json
      actions:
        - id: check
          title: Check channel
          title_key: form.check
          rpc: check-channel
```

- `id` is kebab case and unique in the section, `title` (at most 40 characters) is required, `title_key` translates it,
  and `rpc` must name an entry of `ui.rpc_exports`.
- The payload is `{"form": "input" | "output", "section": "<section id>", "action": "<action id>", "config": {…}}`,
  with `config` the unsaved `plugin_config`. An export with an `input_schema` must accept that shape.
- The answer is shown under the buttons: a string as text, `{"message": "…", "ok": false}` as its message (an error
  when `ok` is `false`), `null` as "Done.", anything else as formatted JSON. A failed call shows its error. Up to
  4,000 characters are shown.
- A `kind: write` export needs write access: its button is disabled for a user who may only read.

## Localisation of the form

A `title` or `description` in the schema that is a key of the plugin's locale files is shown translated, and so are
`x-placeholder`, `x-pattern-message` and the entries of `x-enum-labels`; any other value is shown as written, so a
plugin without locale files keeps plain text. List the languages in `ui.locales` and
ship one flat file per language in `ui/locales/<lang>.json` (the file format and the `en` fallback are described in
[Custom UI](./custom-ui.md#localisation)). A key missing from a language falls back to `en`.

```json
{ "properties": { "url": { "title": "settings.url.title", "description": "settings.url.description" } } }
```

`plugins/notifiers/discord-notifier` is a complete example: every schema text is a key, in `en`, `ru`, `es` and `ar`.
