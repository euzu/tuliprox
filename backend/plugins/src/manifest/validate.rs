//! Raw manifest -> typed [`Manifest`], collecting every violation.

use super::{
    hook::{HookId, Requirement},
    raw::{
        RawConfigExtension, RawConfigTab, RawContribution, RawFormAction, RawFormSection, RawManifest, RawRpcExport,
        RawSourceTile, RawTask, RawUi,
    },
    Concurrency, ConfigExtension, ConfigSection, ConfigTab, ConfigTabSource, Contribution, FailMode, FormAction,
    FormSection, Manifest, Provides, Resources, RpcExport, RpcKind, Scope, Slot, SourceTile, Task, Ui,
    HOST_API_VERSION,
};
use regex::Regex;
use semver::Version;
use serde_json::Value;
use shared::model::{is_kebab_id, is_valid_plugin_category, is_valid_plugin_id, EventKindMask, PlaylistItemType};
use std::{
    collections::HashSet,
    path::{Component, Path},
};

/// `^[a-z][a-z0-9-]{1,40}$`
const TASK_ID_LEN: std::ops::RangeInclusive<usize> = 2..=41;
/// Output format ids and rpc names: kebab case, `^[a-z][a-z0-9-]{0,63}$`.
const NAME_LEN: std::ops::RangeInclusive<usize> = 1..=64;
/// The longest `ui.input_tile.title` / `ui.output_tile.title`, in chars: a
/// sidebar tile has room for a short label only.
const TILE_TITLE_CHARS: usize = 40;

/// Collects violations and tells apart "absent" from "present but wrong".
struct Errors(Vec<String>);

impl Errors {
    fn push(&mut self, message: impl Into<String>) { self.0.push(message.into()); }
}

pub(super) fn build(raw: RawManifest, dir_name: &str) -> Result<Manifest, Vec<String>> {
    let mut errors = Errors(Vec::new());

    let id = check_id(raw.id, dir_name, &mut errors);
    let name = required_text("name", raw.name, &mut errors);
    let version = check_version("version", raw.version, true, &mut errors);
    let api_version = check_api_version(raw.api_version, &mut errors);
    let min_tuliprox_version = check_version("min_tuliprox_version", raw.min_tuliprox_version, false, &mut errors);

    let category = check_category(raw.category, &mut errors);
    let hooks = check_hooks(&raw.hooks, &mut errors);
    let (events, event_names) = check_events(raw.events, &mut errors);
    let tasks = check_tasks(raw.tasks, &mut errors);
    let provides = check_provides(raw.provides.input_type, raw.provides.output_format, &mut errors);
    let scope = check_scope(raw.scope.groups, &raw.scope.item_types, &mut errors);
    let ui = check_ui(raw.ui, &mut errors);
    let resources = check_resources(&raw.resources, &mut errors);

    let manifest = Manifest {
        id: id.unwrap_or_default(),
        name: name.unwrap_or_default(),
        version: version.unwrap_or_else(|| Version::new(0, 0, 0)),
        api_version: api_version.unwrap_or(HOST_API_VERSION),
        author: non_blank(raw.author),
        homepage: non_blank(raw.homepage),
        description: non_blank(raw.description),
        category,
        min_tuliprox_version,
        capabilities: raw.capabilities,
        hooks,
        events,
        event_names,
        tasks,
        provides,
        scope,
        ui,
        resources,
    };
    check_hook_requirements(&manifest, &mut errors);
    check_declarations_have_hooks(&manifest, &mut errors);
    check_title_keys(&manifest, &mut errors);

    if errors.0.is_empty() {
        Ok(manifest)
    } else {
        Err(errors.0)
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// `category:` is one lowercase id (`notifier`, `epg`, ...). It is not normalised here: a manifest is
/// written once by the author, and the id it declares is the id a catalog and the UI will show.
fn check_category(value: Option<String>, errors: &mut Errors) -> Option<String> {
    let category = non_blank(value)?;
    if !is_valid_plugin_category(&category) {
        errors.push(format!(
            "`category` `{category}` is invalid; use 2 to 32 lowercase letters, digits and `-`, starting with a letter"
        ));
        return None;
    }
    Some(category)
}

fn required_text(field: &str, value: Option<String>, errors: &mut Errors) -> Option<String> {
    let text = non_blank(value);
    if text.is_none() {
        errors.push(format!("`{field}` is required"));
    }
    text
}

fn check_id(id: Option<String>, dir_name: &str, errors: &mut Errors) -> Option<String> {
    let Some(id) = id else {
        errors.push("`id` is required");
        return None;
    };
    let mut valid = true;
    if !is_valid_plugin_id(&id) {
        errors.push(format!("id `{id}` is invalid; it must match ^[a-z][a-z0-9-]{{2,40}}$"));
        valid = false;
    }
    if id != dir_name {
        errors.push(format!("id `{id}` does not match its directory `{dir_name}`"));
        valid = false;
    }
    valid.then_some(id)
}

fn check_version(field: &str, value: Option<String>, required: bool, errors: &mut Errors) -> Option<Version> {
    let Some(text) = non_blank(value) else {
        if required {
            errors.push(format!("`{field}` is required"));
        }
        return None;
    };
    match Version::parse(&text) {
        Ok(version) => Some(version),
        Err(err) => {
            errors.push(format!("{field} `{text}` is not a semantic version (major.minor.patch): {err}"));
            None
        }
    }
}

fn check_api_version(value: Option<u32>, errors: &mut Errors) -> Option<u32> {
    match value {
        None => {
            errors.push("`api_version` is required");
            None
        }
        Some(HOST_API_VERSION) => Some(HOST_API_VERSION),
        Some(newer) if newer > HOST_API_VERSION => {
            errors.push(format!(
                "api_version {newer} is newer than this host supports (host ABI major is {HOST_API_VERSION})"
            ));
            None
        }
        Some(older) => {
            errors.push(format!("api_version {older} is not supported: the host ABI major is {HOST_API_VERSION}"));
            None
        }
    }
}

fn check_hooks(names: &[String], errors: &mut Errors) -> Vec<HookId> {
    let mut hooks = Vec::with_capacity(names.len());
    for name in names {
        match HookId::parse(name) {
            Ok(HookId::EpgResolveConflict) => {
                errors.push(
                    "hook `epg.resolve-conflict` is not supported yet: planned for a later ABI 1.x release".to_string(),
                );
            }
            Ok(hook) => {
                if !hooks.contains(&hook) {
                    hooks.push(hook);
                }
            }
            Err(message) => errors.push(message),
        }
    }
    hooks
}

fn check_events(names: Vec<String>, errors: &mut Errors) -> (EventKindMask, Vec<String>) {
    let (mask, unknown) = EventKindMask::from_wire_names(names.iter().map(String::as_str));
    if !unknown.is_empty() {
        let list = unknown.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ");
        errors.push(format!("events lists unknown event names: {list}"));
    }
    (mask, names)
}

fn check_tasks(raw: Vec<RawTask>, errors: &mut Errors) -> Vec<Task> {
    let mut seen = HashSet::new();
    let mut tasks = Vec::with_capacity(raw.len());
    for (index, task) in raw.into_iter().enumerate() {
        let label = task.id.as_deref().map_or_else(|| format!("tasks[{index}]"), |id| format!("task `{id}`"));
        let id = match task.id {
            None => {
                errors.push(format!("{label}: `id` is required"));
                continue;
            }
            Some(id) if !is_kebab_id(&id, TASK_ID_LEN) => {
                errors.push(format!("{label}: id is invalid; it must match ^[a-z][a-z0-9-]{{1,40}}$"));
                continue;
            }
            Some(id) => id,
        };
        if !seen.insert(id.clone()) {
            errors.push(format!("task id `{id}` is used more than once"));
            continue;
        }
        let concurrency = match task.concurrency.as_deref() {
            None | Some("skip") => Concurrency::Skip,
            Some("queue") => Concurrency::Queue,
            Some(other) => {
                errors.push(format!("{label}: concurrency `{other}` is not one of skip, queue"));
                Concurrency::Skip
            }
        };
        if task.max_exec_ms == Some(0) {
            errors.push(format!("{label}: max_exec_ms must not be zero"));
        }
        tasks.push(Task {
            title: non_blank(task.title).unwrap_or_else(|| id.clone()),
            id,
            default_schedule: non_blank(task.default_schedule),
            max_exec_ms: task.max_exec_ms,
            concurrency,
            respect_maintenance: task.respect_maintenance.unwrap_or(true),
        });
    }
    tasks
}

fn check_provides(input_type: bool, output_format: Option<String>, errors: &mut Errors) -> Provides {
    let output_format = output_format.map(|format| format.trim().to_string());
    if let Some(format) = output_format.as_deref() {
        if !is_kebab_id(format, NAME_LEN) {
            errors.push(format!("provides.output_format `{format}` is invalid; use lowercase letters, digits and `-`"));
        }
    }
    Provides { input_type, output_format }
}

/// `movie` is the name of the `Video` item type in `scope.item_types`.
fn parse_item_type(text: &str) -> Option<PlaylistItemType> {
    let name = text.trim().replace('-', "_");
    if name == "movie" {
        return Some(PlaylistItemType::Video);
    }
    serde_json::from_value(Value::String(name)).ok()
}

fn check_scope(groups: Vec<String>, item_types: &[String], errors: &mut Errors) -> Scope {
    for pattern in &groups {
        if let Err(err) = Regex::new(pattern) {
            errors.push(format!("scope.groups pattern `{pattern}` is not a valid regex: {err}"));
        }
    }
    let mut parsed = Vec::with_capacity(item_types.len());
    for text in item_types {
        match parse_item_type(text) {
            Some(item_type) => {
                if !parsed.contains(&item_type) {
                    parsed.push(item_type);
                }
            }
            None => errors.push(format!("scope.item_types `{text}` is not a playlist item type")),
        }
    }
    Scope { groups, item_types: parsed }
}

/// A path inside the package: relative, no `..`, no backslashes.
fn is_package_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && Path::new(path).components().all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        && Path::new(path).components().any(|component| matches!(component, Component::Normal(_)))
}

fn check_path(field: &str, path: &str, errors: &mut Errors) -> bool {
    let valid = is_package_path(path);
    if !valid {
        errors.push(format!("{field} `{path}` must be a relative path inside the package (no `..`, no absolute path)"));
    }
    valid
}

fn check_ui(raw: RawUi, errors: &mut Errors) -> Ui {
    let config_section = raw.config_section.and_then(|section| {
        let Some(schema) = non_blank(section.schema) else {
            errors.push("ui.config_section.schema is required");
            return None;
        };
        check_path("ui.config_section.schema", &schema, errors).then_some(ConfigSection { schema })
    });
    let config_extensions = check_config_extensions(raw.config_extensions, errors);
    let config_tab = raw.config_tab.and_then(|tab| check_config_tab(tab, errors));
    let locales = check_locales(raw.locales, errors);
    let contributions =
        raw.contributions.into_iter().enumerate().filter_map(|(i, c)| check_contribution(i, c, errors)).collect();
    let rpc_exports = check_rpc_exports(raw.rpc_exports, errors);
    let input_form = check_form_sections(raw.input_form, "ui.input_form", errors);
    let output_form = check_form_sections(raw.output_form, "ui.output_form", errors);
    let input_tile = raw.input_tile.map(|tile| check_source_tile(tile, "ui.input_tile", errors));
    let output_tile = raw.output_tile.map(|tile| check_source_tile(tile, "ui.output_tile", errors));
    let ui = Ui {
        config_section,
        config_extensions,
        config_tab,
        locales,
        contributions,
        rpc_exports,
        input_form,
        output_form,
        input_tile,
        output_tile,
    };
    check_form_action_rpcs(&ui, errors);
    ui
}

/// `ui.input_tile` / `ui.output_tile`: every field is optional, a title must
/// fit on a sidebar tile and must not carry control characters.
fn check_source_tile(raw: RawSourceTile, field: &str, errors: &mut Errors) -> SourceTile {
    let title = non_blank(raw.title).filter(|title| {
        if title.chars().any(char::is_control) {
            errors.push(format!("{field}.title must not contain control characters"));
            false
        } else if title.chars().count() > TILE_TITLE_CHARS {
            errors.push(format!("{field}.title is longer than {TILE_TITLE_CHARS} characters"));
            false
        } else {
            true
        }
    });
    SourceTile {
        title,
        title_key: non_blank(raw.title_key),
        icon: non_blank(raw.icon),
        description: non_blank(raw.description),
        description_key: non_blank(raw.description_key),
    }
}

/// The sections of one form: an id (unique in the form), an optional title and a
/// package-relative schema path.
fn check_form_sections(raw: Vec<RawFormSection>, field: &str, errors: &mut Errors) -> Vec<FormSection> {
    let mut sections: Vec<FormSection> = Vec::with_capacity(raw.len());
    for (index, section) in raw.into_iter().enumerate() {
        let Some(id) = non_blank(section.id) else {
            errors.push(format!("{field}[{index}]: `id` is required"));
            continue;
        };
        if !is_kebab_id(&id.replace('_', "-"), NAME_LEN) {
            errors.push(format!("{field}[{index}]: id `{id}` is not a panel id"));
            continue;
        }
        if sections.iter().any(|existing| existing.id == id) {
            errors.push(format!("{field}[{index}]: id `{id}` is declared twice"));
            continue;
        }
        let title = non_blank(section.title);
        let title_key = non_blank(section.title_key);
        let Some(schema) = non_blank(section.schema) else {
            errors.push(format!("{field} `{id}`: `schema` is required"));
            continue;
        };
        if !check_path(&format!("{field} `{id}` schema"), &schema, errors) {
            continue;
        }
        let actions = check_form_actions(section.actions, &format!("{field} `{id}`"), errors);
        sections.push(FormSection {
            id,
            title,
            title_key,
            icon: non_blank(section.icon),
            description: non_blank(section.description),
            description_key: non_blank(section.description_key),
            schema,
            actions,
        });
    }
    sections
}

/// The `actions` of one form section: an id unique in the section, a title that
/// fits on a button and the rpc export it calls. Whether that export exists is
/// checked by [`check_form_action_rpcs`], once every export is known.
fn check_form_actions(raw: Vec<RawFormAction>, field: &str, errors: &mut Errors) -> Vec<FormAction> {
    let mut actions: Vec<FormAction> = Vec::with_capacity(raw.len());
    for (index, action) in raw.into_iter().enumerate() {
        let Some(id) = non_blank(action.id) else {
            errors.push(format!("{field} actions[{index}]: `id` is required"));
            continue;
        };
        if !is_kebab_id(&id.replace('_', "-"), NAME_LEN) {
            errors.push(format!("{field} actions[{index}]: id `{id}` is not an action id"));
            continue;
        }
        if actions.iter().any(|existing| existing.id == id) {
            errors.push(format!("{field} actions[{index}]: id `{id}` is declared twice"));
            continue;
        }
        let Some(title) = non_blank(action.title) else {
            errors.push(format!("{field} action `{id}`: `title` is required"));
            continue;
        };
        if title.chars().any(char::is_control) || title.chars().count() > TILE_TITLE_CHARS {
            errors.push(format!(
                "{field} action `{id}`: `title` must be at most {TILE_TITLE_CHARS} characters without control characters"
            ));
            continue;
        }
        let Some(rpc) = non_blank(action.rpc) else {
            errors.push(format!("{field} action `{id}`: `rpc` is required"));
            continue;
        };
        actions.push(FormAction { id, title, title_key: non_blank(action.title_key), rpc });
    }
    actions
}

/// Every form action names an entry of `ui.rpc_exports`.
fn check_form_action_rpcs(ui: &Ui, errors: &mut Errors) {
    for (field, sections) in [("ui.input_form", &ui.input_form), ("ui.output_form", &ui.output_form)] {
        for section in sections {
            for action in &section.actions {
                if !ui.rpc_exports.iter().any(|export| export.name == action.rpc) {
                    errors.push(format!(
                        "{field} `{}` action `{}`: rpc `{}` is not declared in ui.rpc_exports",
                        section.id, action.id, action.rpc
                    ));
                }
            }
        }
    }
}

fn check_config_extensions(raw: Vec<RawConfigExtension>, errors: &mut Errors) -> Vec<ConfigExtension> {
    let mut extensions = Vec::with_capacity(raw.len());
    for (index, extension) in raw.into_iter().enumerate() {
        let Some(section) = non_blank(extension.section) else {
            errors.push(format!("ui.config_extensions[{index}]: `section` is required"));
            continue;
        };
        if !is_kebab_id(&section.replace('_', "-"), NAME_LEN) {
            errors.push(format!("ui.config_extensions[{index}]: section `{section}` is not a config section id"));
            continue;
        }
        match extension.schema {
            Some(schema @ Value::Object(_)) => extensions.push(ConfigExtension { section, schema }),
            Some(_) => errors.push(format!("ui.config_extensions `{section}`: `schema` must be a JSON Schema object")),
            None => errors.push(format!("ui.config_extensions `{section}`: `schema` is required")),
        }
    }
    extensions
}

fn check_config_tab(tab: RawConfigTab, errors: &mut Errors) -> Option<ConfigTab> {
    let title = non_blank(tab.title);
    let title_key = non_blank(tab.title_key);
    if title.is_none() && title_key.is_none() {
        errors.push("ui.config_tab needs a `title` or a `title_key`");
    }
    let source = match (non_blank(tab.schema), non_blank(tab.element)) {
        (Some(schema), None) => {
            check_path("ui.config_tab.schema", &schema, errors).then_some(ConfigTabSource::Schema(schema))
        }
        (None, Some(element)) => {
            check_path("ui.config_tab.element", &element, errors).then_some(ConfigTabSource::Element(element))
        }
        (Some(_), Some(_)) => {
            errors.push("ui.config_tab must declare exactly one of `schema` or `element`, not both");
            None
        }
        (None, None) => {
            errors.push("ui.config_tab must declare exactly one of `schema` or `element`");
            None
        }
    };
    source.map(|source| ConfigTab { title, title_key, icon: non_blank(tab.icon), source })
}

fn check_locales(raw: Vec<String>, errors: &mut Errors) -> Vec<String> {
    let mut locales = Vec::with_capacity(raw.len());
    for locale in raw {
        let valid = !locale.is_empty()
            && locale.len() <= 16
            && locale.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            errors.push(format!("ui.locales `{locale}` is not a language code such as `en` or `pt-BR`"));
        } else if !locales.contains(&locale) {
            locales.push(locale);
        }
    }
    locales
}

fn check_contribution(index: usize, raw: RawContribution, errors: &mut Errors) -> Option<Contribution> {
    let label = format!("ui.contributions[{index}]");
    let slot = match raw.slot.as_deref() {
        None => {
            errors.push(format!("{label}: `slot` is required"));
            None
        }
        Some(name) => {
            let slot = Slot::parse(name);
            if slot.is_none() {
                let known = Slot::ALL.map(Slot::as_str).join(", ");
                errors.push(format!("{label}: unknown slot `{name}`; known slots are {known}"));
            }
            slot
        }
    };
    let title = non_blank(raw.title);
    if title.is_none() {
        errors.push(format!("{label}: `title` is required (it is the fallback for `title_key`)"));
    }
    let element = match non_blank(raw.element) {
        None => {
            errors.push(format!("{label}: `element` is required"));
            None
        }
        Some(element) => check_path(&format!("{label}.element"), &element, errors).then_some(element),
    };
    Some(Contribution {
        slot: slot?,
        title: title?,
        title_key: non_blank(raw.title_key),
        icon: non_blank(raw.icon),
        element: element?,
    })
}

fn check_rpc_exports(raw: Vec<RawRpcExport>, errors: &mut Errors) -> Vec<RpcExport> {
    let mut seen = HashSet::new();
    let mut exports = Vec::with_capacity(raw.len());
    for (index, export) in raw.into_iter().enumerate() {
        let label =
            export.name.as_deref().map_or_else(|| format!("ui.rpc_exports[{index}]"), |n| format!("rpc export `{n}`"));
        let Some(name) = export.name else {
            errors.push(format!("{label}: `name` is required"));
            continue;
        };
        if !is_kebab_id(&name, NAME_LEN) {
            errors.push(format!("{label}: name is invalid; use lowercase letters, digits and `-`"));
            continue;
        }
        if !seen.insert(name.clone()) {
            errors.push(format!("rpc export name `{name}` is used more than once"));
            continue;
        }
        let kind = match export.kind.as_deref() {
            Some("read") => Some(RpcKind::Read),
            Some("write") => Some(RpcKind::Write),
            Some(other) => {
                errors.push(format!("{label}: kind `{other}` is not one of read, write"));
                None
            }
            None => {
                errors.push(format!("{label}: `kind` is required (read or write)"));
                None
            }
        };
        let input_schema = non_blank(export.input_schema);
        match input_schema.as_deref() {
            Some(path) => {
                check_path(&format!("{label}: input_schema"), path, errors);
            }
            None if export.mcp => errors.push(format!("{label}: `input_schema` is required when `mcp: true`")),
            None => {}
        }
        if let Some(kind) = kind {
            exports.push(RpcExport {
                name,
                kind,
                mcp: export.mcp,
                description: non_blank(export.description),
                input_schema,
            });
        }
    }
    exports
}

fn check_resources(raw: &super::raw::RawResources, errors: &mut Errors) -> Resources {
    if raw.max_memory_mb == Some(0) {
        errors.push("resources.max_memory_mb must not be zero");
    }
    if raw.max_exec_ms == Some(0) {
        errors.push("resources.max_exec_ms must not be zero");
    }
    if raw.max_instances == Some(0) {
        errors.push("resources.max_instances must not be zero");
    }
    let fail_mode = match raw.fail_mode.as_deref() {
        None | Some("continue") => FailMode::Continue,
        Some("abort-target") => FailMode::AbortTarget,
        Some(other) => {
            errors.push(format!("resources.fail_mode `{other}` is not one of continue, abort-target"));
            FailMode::Continue
        }
    };
    Resources {
        max_memory_mb: raw.max_memory_mb,
        max_exec_ms: raw.max_exec_ms,
        max_instances: raw.max_instances,
        fail_mode,
    }
}

/// The "Also requires" column of the hook table.
fn check_hook_requirements(manifest: &Manifest, errors: &mut Errors) {
    for hook in &manifest.hooks {
        let missing = match hook.requirement() {
            Requirement::None => None,
            Requirement::Capability(capability) => (!manifest.capabilities.has(capability))
                .then(|| format!("`capabilities.{}: true`", capability.as_str())),
            Requirement::Events => manifest.event_names.is_empty().then(|| "a non-empty `events:` list".to_string()),
            Requirement::InputType => {
                (!manifest.provides.input_type).then(|| "`provides.input_type: true`".to_string())
            }
            Requirement::OutputFormat => {
                manifest.provides.output_format.is_none().then(|| "`provides.output_format`".to_string())
            }
            Requirement::RpcExports => {
                manifest.ui.rpc_exports.is_empty().then(|| "a non-empty `ui.rpc_exports` list".to_string())
            }
            Requirement::Tasks => manifest.tasks.is_empty().then(|| "a non-empty `tasks:` list".to_string()),
        };
        if let Some(missing) = missing {
            errors.push(format!("hook `{}` requires {missing}", hook.as_str()));
        }
    }
}

/// The other direction: what the manifest declares only makes sense with the
/// hook that serves it, and an id is listed iff the component exports it.
fn check_declarations_have_hooks(manifest: &Manifest, errors: &mut Errors) {
    let mut orphan = |declared: bool, what: &str, hook: HookId| {
        if declared && !manifest.has_hook(hook) {
            errors.push(format!("{what} is declared but hook `{}` is not listed", hook.as_str()));
        }
    };
    orphan(!manifest.event_names.is_empty(), "`events:`", HookId::EventsOnEvent);
    orphan(!manifest.tasks.is_empty(), "`tasks:`", HookId::TasksRun);
    orphan(!manifest.ui.rpc_exports.is_empty(), "`ui.rpc_exports`", HookId::RpcCall);
    orphan(manifest.provides.input_type, "`provides.input_type`", HookId::InputFetch);
    orphan(manifest.provides.output_format.is_some(), "`provides.output_format`", HookId::OutputRender);
    orphan(manifest.ui.input_tile.is_some(), "`ui.input_tile`", HookId::InputFetch);
    orphan(manifest.ui.output_tile.is_some(), "`ui.output_tile`", HookId::OutputRender);
}

/// `title_key`s resolve in `ui/locales`, so they need at least one locale.
fn check_title_keys(manifest: &Manifest, errors: &mut Errors) {
    if !manifest.ui.locales.is_empty() {
        return;
    }
    let tab_key = manifest.ui.config_tab.iter().filter_map(|tab| tab.title_key.as_deref());
    let slot_keys = manifest.ui.contributions.iter().filter_map(|contribution| contribution.title_key.as_deref());
    for key in tab_key.chain(slot_keys) {
        errors.push(format!("title_key `{key}` needs `ui.locales` to resolve in"));
    }
    for (field, sections) in [("ui.input_form", &manifest.ui.input_form), ("ui.output_form", &manifest.ui.output_form)]
    {
        for section in sections {
            let action_keys = section.actions.iter().map(|action| ("action title_key", &action.title_key));
            let keys = [("title_key", &section.title_key), ("description_key", &section.description_key)];
            for (name, key) in keys.into_iter().chain(action_keys) {
                if let Some(key) = key {
                    errors.push(format!("{field} `{}` {name} `{key}` needs `ui.locales` to resolve in", section.id));
                }
            }
        }
    }
    for (field, tile) in [("ui.input_tile", &manifest.ui.input_tile), ("ui.output_tile", &manifest.ui.output_tile)] {
        let Some(tile) = tile else { continue };
        for (name, key) in [("title_key", &tile.title_key), ("description_key", &tile.description_key)] {
            if let Some(key) = key {
                errors.push(format!("{field}.{name} `{key}` needs `ui.locales` to resolve in"));
            }
        }
    }
}
