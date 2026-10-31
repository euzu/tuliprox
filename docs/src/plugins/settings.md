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
4. Only then is `config.yml` written, with a backup of the previous file, and the plugins are reloaded.

The answer is `200` with the masked settings, the schema, the secret list and the `warnings`. A `503` means no
instance was free within the call deadline.

The saved values appear in `config.yml` under `plugins.plugins[]`, in that entry's `settings` map. The file is
rewritten from the parsed config, so comments in it are not kept.

A settings save for a plugin that has no entry creates the entry **disabled**. It does not enable the plugin, and it
does not approve it. See [Security Model](./security.md#trust-on-first-use).

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
