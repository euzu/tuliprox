use super::*;
use shared::{error::ErrorKind, model::EventKind};

/// A complete, self-consistent manifest: `stream.inject-headers` needs
/// `capabilities.stream_headers`, which is declared here.
const FULL: &str = r#"
id: sports-enricher
name: Sports Channel Enricher
version: 1.2.0
api_version: 1
author: Jane Doe <jane@example.com>
homepage: https://github.com/janedoe/tuliprox-sports-enricher
description: >
  Groups sport channels by sport, enriches EPG with live fixture data.
min_tuliprox_version: "3.4.0"
capabilities:
  network: true
  storage: true
  epg_write: true
  image_cache: true
  notifications: false
  stream_headers: true
hooks:
  - playlist.after-filter
  - playlist.after-sort
  - playlist.enrich-metadata
  - playlist.sort-key
  - playlist.assign-chno
  - playlist.resolve-duplicates
  - playlist.merge-groups
  - filter.eval-fn
  - stream.inject-headers
  - epg.enrich-programmes
  - epg.match-channel
  - epg.transform-programmes
  - events.on-event
  - metrics.collect
  - tasks.run
  - rpc.call
events:
  - playlist.update
  - user.connection.changed
  - stream.probe.failed
tasks:
  - id: refresh-fixtures
    title: "Refresh fixture cache"
    default_schedule: "0 0 4 * * *"
    max_exec_ms: 60000
    concurrency: skip
    respect_maintenance: true
scope:
  groups: ["(?i)sport"]
  item_types: [live]
ui:
  config_section:
    schema: config-schema.json
  config_extensions:
    - section: webhooks
      schema:
        type: object
        properties:
          notify_sports_events: { type: boolean, title: "Notify on sport events" }
  config_tab:
    title: "Sports"
    icon: trophy
    schema: sports-tab-schema.json
  locales: [en, de, fr]
  contributions:
    - slot: menu
      title: "Sports Guide"
      title_key: menu.title
      icon: trophy
      element: ui/sports-guide.html
    - slot: source-editor-tile
      title: "Sports Enricher"
      element: ui/sports-tile.html
  rpc_exports:
    - name: get-fixtures
      kind: read
      mcp: true
      description: "Upcoming fixtures for the tracked leagues"
      input_schema: rpc/get-fixtures.schema.json
resources:
  max_memory_mb: 64
  max_exec_ms: 5000
  max_instances: 4
  fail_mode: continue
"#;

const MINIMAL: &str = "id: demo-plugin\nname: Demo\nversion: 0.1.0\napi_version: 1\n";

fn parse(text: &str) -> Result<Manifest, TuliproxError> { Manifest::parse(text, "demo-plugin") }

/// `MINIMAL` plus extra top-level YAML.
fn with(extra: &str) -> String { format!("{MINIMAL}{extra}") }

/// The one-line-per-problem message of a rejected manifest.
fn rejected(text: &str) -> String { parse(text).expect_err("manifest should be rejected").to_string() }

fn problems(text: &str) -> Vec<String> {
    let message = rejected(text);
    message.lines().skip(1).map(|line| line.trim().trim_start_matches("- ").to_string()).collect()
}

fn assert_problem(text: &str, needle: &str) {
    let found = problems(text);
    assert!(found.iter().any(|line| line.contains(needle)), "no problem contains `{needle}`: {found:#?}");
}

#[test]
fn the_full_example_parses_into_typed_fields() {
    let manifest = Manifest::parse(FULL, "sports-enricher").unwrap();
    assert_eq!(manifest.id, "sports-enricher");
    assert_eq!(manifest.name, "Sports Channel Enricher");
    assert_eq!(manifest.version, Version::new(1, 2, 0));
    assert_eq!(manifest.api_version, 1);
    assert_eq!(manifest.author.as_deref(), Some("Jane Doe <jane@example.com>"));
    assert_eq!(manifest.homepage.as_deref(), Some("https://github.com/janedoe/tuliprox-sports-enricher"));
    assert_eq!(
        manifest.description.as_deref(),
        Some("Groups sport channels by sport, enriches EPG with live fixture data.")
    );
    assert_eq!(manifest.min_tuliprox_version, Some(Version::new(3, 4, 0)));

    assert_eq!(
        manifest.capabilities,
        Capabilities {
            network: true,
            storage: true,
            epg_write: true,
            image_cache: true,
            notifications: false,
            stream_headers: true
        }
    );
    assert_eq!(manifest.capabilities.granted().len(), 5);

    assert_eq!(manifest.hooks.len(), 16);
    assert_eq!(manifest.hooks[0], HookId::PlaylistAfterFilter);
    assert!(manifest.has_hook(HookId::RpcCall));
    assert!(!manifest.has_hook(HookId::EpgLoaded));

    assert_eq!(manifest.event_names, ["playlist.update", "user.connection.changed", "stream.probe.failed"]);
    assert!(manifest.events.contains(EventKind::PlaylistUpdate));
    assert!(manifest.events.contains(EventKind::StreamProbeFailed));
    assert!(!manifest.events.contains(EventKind::ScheduledTaskFailed));

    assert_eq!(
        manifest.tasks,
        [Task {
            id: "refresh-fixtures".to_string(),
            title: "Refresh fixture cache".to_string(),
            default_schedule: Some("0 0 4 * * *".to_string()),
            max_exec_ms: Some(60_000),
            concurrency: Concurrency::Skip,
            respect_maintenance: true,
        }]
    );

    assert_eq!(manifest.provides, Provides::default());
    assert_eq!(manifest.scope.groups, ["(?i)sport"]);
    assert_eq!(manifest.scope.item_types, [PlaylistItemType::Live]);

    let ui = &manifest.ui;
    assert_eq!(ui.config_section, Some(ConfigSection { schema: "config-schema.json".to_string() }));
    assert_eq!(ui.config_extensions.len(), 1);
    assert_eq!(ui.config_extensions[0].section, "webhooks");
    assert_eq!(ui.config_extensions[0].schema["properties"]["notify_sports_events"]["type"], "boolean");
    let tab = ui.config_tab.as_ref().unwrap();
    assert_eq!(tab.title.as_deref(), Some("Sports"));
    assert_eq!(tab.icon.as_deref(), Some("trophy"));
    assert_eq!(tab.source, ConfigTabSource::Schema("sports-tab-schema.json".to_string()));
    assert_eq!(ui.locales, ["en", "de", "fr"]);
    assert_eq!(ui.contributions.len(), 2);
    assert_eq!(ui.contributions[0].slot, Slot::Menu);
    assert_eq!(ui.contributions[0].title_key.as_deref(), Some("menu.title"));
    assert_eq!(ui.contributions[1].slot, Slot::SourceEditorTile);
    assert_eq!(
        ui.rpc_exports,
        [RpcExport {
            name: "get-fixtures".to_string(),
            kind: RpcKind::Read,
            mcp: true,
            description: Some("Upcoming fixtures for the tracked leagues".to_string()),
            input_schema: Some("rpc/get-fixtures.schema.json".to_string()),
        }]
    );

    assert_eq!(
        manifest.resources,
        Resources {
            max_memory_mb: Some(64),
            max_exec_ms: Some(5_000),
            max_instances: Some(4),
            fail_mode: FailMode::Continue
        }
    );
}

#[test]
fn a_minimal_manifest_has_defaults_everywhere() {
    let manifest = parse(MINIMAL).unwrap();
    assert_eq!(manifest.id, "demo-plugin");
    assert_eq!(manifest.hooks.len(), 0);
    assert_eq!(manifest.capabilities, Capabilities::default());
    assert_eq!(manifest.capabilities.granted().len(), 0);
    assert_eq!(manifest.events, EventKindMask::NONE);
    assert_eq!(manifest.tasks.len(), 0);
    assert_eq!(manifest.provides, Provides::default());
    assert_eq!(manifest.scope, Scope::default());
    assert_eq!(manifest.ui, Ui::default());
    assert_eq!(manifest.resources, Resources::default());
    assert_eq!(manifest.resources.fail_mode, FailMode::Continue);
    assert_eq!(manifest.pipeline_hooks().len(), 0);
}

#[test]
fn keys_present_but_empty_read_as_defaults() {
    // Keys present but empty, as left by the example with its `# provides:` block commented out
    let manifest =
        parse(&with("hooks:\nevents:\ntasks:\nprovides:\nscope:\nui:\nresources:\ncapabilities:\n")).unwrap();
    assert_eq!(manifest.hooks.len(), 0);
    assert_eq!(manifest.resources, Resources::default());
}

#[test]
fn unknown_keys_are_ignored() {
    let manifest = parse(&with("future_feature: 7\nui:\n  somewhere_new: true\n")).unwrap();
    assert_eq!(manifest.ui, Ui::default());
}

#[test]
fn missing_required_fields_are_all_reported_together() {
    let found = problems("hooks: []\n");
    assert_eq!(found.len(), 4, "{found:?}");
    for field in ["`id`", "`name`", "`version`", "`api_version`"] {
        assert!(found.iter().any(|line| line.contains(field)), "{field} missing from {found:?}");
    }
}

#[test]
fn malformed_yaml_is_a_single_plugin_error() {
    let err = parse("id: [unclosed\n").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Plugin);
    assert!(parse("").is_err());
    assert!(parse("- a\n- b\n").is_err());
}

#[test]
fn errors_carry_the_plugin_kind_and_name_the_directory() {
    let err = parse("id: x\n").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Plugin);
    assert!(err.to_string().contains("`demo-plugin`"), "{err}");
}

#[test]
fn id_must_match_the_regex_and_the_directory() {
    assert_problem(&MINIMAL.replace("demo-plugin", "Demo"), "id `Demo` is invalid");
    assert_problem(&MINIMAL.replace("demo-plugin", "ab"), "id `ab` is invalid");
    assert_problem(&MINIMAL.replace("demo-plugin", "1demo"), "is invalid");
    assert_problem(&MINIMAL.replace("demo-plugin", "demo_plugin"), "is invalid");
    assert_problem(&MINIMAL.replace("demo-plugin", "other-plugin"), "does not match its directory `demo-plugin`");
    let longest = format!("a{}", "b".repeat(40));
    assert!(Manifest::parse(&MINIMAL.replace("demo-plugin", &longest), &longest).is_ok());
    let too_long = format!("a{}", "b".repeat(41));
    assert!(Manifest::parse(&MINIMAL.replace("demo-plugin", &too_long), &too_long).is_err());
}

#[test]
fn version_must_be_semver() {
    for bad in ["1.2", "one", "1.2.3.4", "v1.2.3", "\"\""] {
        let text = MINIMAL.replace("0.1.0", bad);
        assert!(parse(&text).is_err(), "{bad}");
    }
    assert_problem(&MINIMAL.replace("0.1.0", "1.2"), "version `1.2` is not a semantic version");
    let manifest = parse(&MINIMAL.replace("0.1.0", "2.0.0-beta.1+build5")).unwrap();
    assert_eq!(manifest.version, Version::parse("2.0.0-beta.1+build5").unwrap());
}

#[test]
fn min_tuliprox_version_is_optional_semver_and_gates_the_host() {
    assert_problem(&with("min_tuliprox_version: soon\n"), "min_tuliprox_version `soon`");
    let manifest = parse(&with("min_tuliprox_version: \"3.4.0\"\n")).unwrap();
    assert!(manifest.requires_newer_host(&Version::new(3, 3, 170)));
    assert!(!manifest.requires_newer_host(&Version::new(3, 4, 0)));
    assert!(!manifest.requires_newer_host(&Version::new(4, 0, 0)));
    assert!(!parse(MINIMAL).unwrap().requires_newer_host(&Version::new(0, 0, 1)));
}

#[test]
fn api_version_must_equal_the_host_abi_major() {
    assert!(parse(MINIMAL).is_ok());
    assert_problem(&MINIMAL.replace("api_version: 1", "api_version: 2"), "api_version 2 is newer");
    assert_problem(&MINIMAL.replace("api_version: 1", "api_version: 0"), "api_version 0 is not supported");
    assert!(parse(&MINIMAL.replace("api_version: 1", "api_version: -1")).is_err());
    assert!(parse(&MINIMAL.replace("api_version: 1", "api_version: one")).is_err());
}

#[test]
fn hook_ids_are_validated_and_duplicates_collapse() {
    let manifest =
        parse(&with("hooks: [playlist.after-filter, playlist.after-filter, playlist.after-sort]\n")).unwrap();
    assert_eq!(manifest.hooks, [HookId::PlaylistAfterFilter, HookId::PlaylistAfterSort]);
    assert_problem(&with("hooks: [playlist.nope]\n"), "unknown hook `playlist.nope`");
    assert_problem(&with("hooks: [Playlist.After-Filter]\n"), "did you mean `playlist.after-filter`");
}

#[test]
fn every_supported_id_is_accepted_when_its_requirements_are_met() {
    let ids = HookId::ALL
        .iter()
        .filter(|hook| **hook != HookId::EpgResolveConflict)
        .map(|hook| ["  - ", hook.as_str(), "\n"].concat())
        .collect::<String>();
    let text = with(&format!(
        "capabilities: {{ epg_write: true, notifications: true, stream_headers: true }}\n\
         hooks:\n{ids}\
         events: [playlist.update]\n\
         tasks: [{{ id: nightly-run }}]\n\
         provides: {{ input_type: true, output_format: my-format }}\n\
         ui:\n  rpc_exports:\n    - {{ name: ping, kind: read }}\n"
    ));
    let manifest = parse(&text).unwrap();
    // Every id but `epg.resolve-conflict`, which is refused until it runs.
    assert_eq!(manifest.hooks.len(), HookId::ALL.len() - 1);
}

#[test]
fn bare_stage_names_are_rejected_naming_the_dotted_form() {
    for stage in ["before-filter", "after-filter", "before-rename", "after-rename", "before-map", "after-map"] {
        assert_problem(&with(&format!("hooks: [{stage}]\n")), &format!("`playlist.{stage}`"));
    }
    // Bare stage names are rejected
    assert_problem("id: demo-plugin\nhooks: [after-filter]\n", "bare stage name");
}

#[test]
fn also_requires_capabilities() {
    for (hook, capability) in [
        ("stream.inject-headers", "stream_headers"),
        ("epg.transform-programmes", "epg_write"),
        ("epg.enrich-programmes", "epg_write"),
        ("notifications.send", "notifications"),
    ] {
        let without = with(&format!("hooks: [{hook}]\n"));
        assert_problem(&without, &format!("hook `{hook}` requires `capabilities.{capability}: true`"));
        let granted = with(&format!("capabilities: {{ {capability}: true }}\nhooks: [{hook}]\n"));
        assert!(parse(&granted).is_ok(), "{hook}: {:?}", parse(&granted));
    }
    // an explicit false is the same as absent
    assert!(parse(&with("capabilities: { stream_headers: false }\nhooks: [stream.inject-headers]\n")).is_err());
}

#[test]
fn also_requires_events_tasks_rpc_exports_and_provides() {
    assert_problem(&with("hooks: [events.on-event]\n"), "requires a non-empty `events:` list");
    assert_problem(&with("hooks: [tasks.run]\n"), "requires a non-empty `tasks:` list");
    assert_problem(&with("hooks: [rpc.call]\n"), "requires a non-empty `ui.rpc_exports` list");
    assert_problem(&with("hooks: [input.fetch]\n"), "requires `provides.input_type: true`");
    assert_problem(&with("hooks: [output.render]\n"), "requires `provides.output_format`");
    assert_problem(
        &with("hooks: [input.fetch]\nprovides: { input_type: false }\n"),
        "requires `provides.input_type: true`",
    );

    assert!(parse(&with("hooks: [events.on-event]\nevents: [playlist.update]\n")).is_ok());
    assert!(parse(&with("hooks: [tasks.run]\ntasks: [{ id: refresh-fixtures }]\n")).is_ok());
    assert!(parse(&with("hooks: [input.fetch]\nprovides: { input_type: true }\n")).is_ok());
    assert!(parse(&with("hooks: [output.render]\nprovides: { output_format: m3u-plus }\n")).is_ok());
    assert!(parse(&with("hooks: [rpc.call]\nui:\n  rpc_exports: [{ name: ping, kind: read }]\n")).is_ok());
}

#[test]
fn declarations_need_the_hook_that_serves_them() {
    assert_problem(&with("events: [playlist.update]\n"), "`events:` is declared but hook `events.on-event`");
    assert_problem(&with("tasks: [{ id: refresh-fixtures }]\n"), "`tasks:` is declared but hook `tasks.run`");
    assert_problem(&with("ui:\n  rpc_exports: [{ name: ping, kind: read }]\n"), "hook `rpc.call` is not listed");
    assert_problem(&with("provides: { input_type: true }\n"), "hook `input.fetch` is not listed");
    assert_problem(&with("provides: { output_format: my-format }\n"), "hook `output.render` is not listed");
}

#[test]
fn event_names_come_from_the_wire_names_and_unknown_ones_are_listed() {
    let text = with("hooks: [events.on-event]\nevents: [playlist.update, playlist.updat, config.changd]\n");
    assert_problem(&text, "unknown event names: `playlist.updat`, `config.changd`");
    let ok = parse(&with("hooks: [events.on-event]\nevents: [playlist.update, scheduled_task.failed]\n")).unwrap();
    assert!(ok.events.contains(EventKind::PlaylistUpdate));
    assert!(ok.events.contains(EventKind::ScheduledTaskFailed));
}

#[test]
fn task_ids_follow_the_regex_and_are_unique() {
    let hooks = "hooks: [tasks.run]\n";
    assert!(parse(&with(&format!("{hooks}tasks: [{{ id: ab }}]\n"))).is_ok());
    for bad in ["a", "Refresh", "1abc", "re_fresh", "-abc"] {
        assert_problem(&with(&format!("{hooks}tasks: [{{ id: {bad} }}]\n")), "id is invalid");
    }
    let too_long = format!("a{}", "b".repeat(41));
    assert_problem(&with(&format!("{hooks}tasks: [{{ id: {too_long} }}]\n")), "id is invalid");
    assert_problem(
        &with(&format!("{hooks}tasks: [{{ id: nightly }}, {{ id: nightly }}]\n")),
        "task id `nightly` is used more than once",
    );
    assert_problem(&with(&format!("{hooks}tasks: [{{ title: untitled }}]\n")), "tasks[0]: `id` is required");
}

#[test]
fn task_defaults_and_concurrency() {
    let manifest =
        parse(&with("hooks: [tasks.run]\ntasks:\n  - id: a-task\n  - { id: b-task, concurrency: queue, respect_maintenance: false }\n"))
            .unwrap();
    assert_eq!(manifest.tasks[0].concurrency, Concurrency::Skip);
    assert!(manifest.tasks[0].respect_maintenance);
    assert_eq!(manifest.tasks[0].title, "a-task", "title falls back to the id");
    assert_eq!(manifest.tasks[1].concurrency, Concurrency::Queue);
    assert!(!manifest.tasks[1].respect_maintenance);
    assert_problem(
        &with("hooks: [tasks.run]\ntasks: [{ id: a-task, concurrency: parallel }]\n"),
        "concurrency `parallel` is not one of skip, queue",
    );
    assert_problem(
        &with("hooks: [tasks.run]\ntasks: [{ id: a-task, max_exec_ms: 0 }]\n"),
        "max_exec_ms must not be zero",
    );
}

#[test]
fn rpc_export_names_are_unique_and_kinds_are_read_or_write() {
    let hook = "hooks: [rpc.call]\nui:\n  rpc_exports:\n";
    assert_problem(
        &with(&format!("{hook}    - {{ name: ping, kind: read }}\n    - {{ name: ping, kind: write }}\n")),
        "rpc export name `ping` is used more than once",
    );
    assert_problem(
        &with(&format!("{hook}    - {{ name: ping, kind: admin }}\n")),
        "kind `admin` is not one of read, write",
    );
    assert_problem(&with(&format!("{hook}    - {{ name: ping }}\n")), "`kind` is required");
    assert_problem(&with(&format!("{hook}    - {{ kind: read }}\n")), "`name` is required");
    assert_problem(&with(&format!("{hook}    - {{ name: getFixtures, kind: read }}\n")), "name is invalid");
    let manifest = parse(&with(&format!("{hook}    - {{ name: reset, kind: write }}\n"))).unwrap();
    assert_eq!(manifest.ui.rpc_exports[0].kind, RpcKind::Write);
    assert!(!manifest.ui.rpc_exports[0].mcp);
}

#[test]
fn mcp_exports_need_an_input_schema() {
    let hook = "hooks: [rpc.call]\nui:\n  rpc_exports:\n";
    assert_problem(
        &with(&format!("{hook}    - {{ name: ping, kind: read, mcp: true }}\n")),
        "`input_schema` is required when `mcp: true`",
    );
    assert!(parse(&with(&format!("{hook}    - {{ name: ping, kind: read, mcp: false }}\n"))).is_ok());
    assert!(parse(&with(&format!(
        "{hook}    - {{ name: ping, kind: read, mcp: true, input_schema: rpc/ping.json }}\n"
    )))
    .is_ok());
}

#[test]
fn config_tab_declares_exactly_one_of_schema_and_element() {
    let schema = parse(&with("ui:\n  config_tab: { title: Tab, schema: tab.json }\n")).unwrap();
    assert_eq!(schema.ui.config_tab.unwrap().source, ConfigTabSource::Schema("tab.json".to_string()));
    let element = parse(&with("ui:\n  config_tab: { title: Tab, element: ui/tab.html }\n")).unwrap();
    assert_eq!(element.ui.config_tab.unwrap().source, ConfigTabSource::Element("ui/tab.html".to_string()));

    assert_problem(
        &with("ui:\n  config_tab: { title: Tab, schema: tab.json, element: ui/tab.html }\n"),
        "exactly one of `schema` or `element`, not both",
    );
    assert_problem(&with("ui:\n  config_tab: { title: Tab }\n"), "exactly one of `schema` or `element`");
    assert_problem(&with("ui:\n  config_tab: { schema: tab.json }\n"), "needs a `title` or a `title_key`");
}

#[test]
fn contribution_slots_are_known() {
    for slot in Slot::ALL {
        let text = with(&format!(
            "ui:\n  contributions:\n    - {{ slot: {}, title: T, element: ui/x.html }}\n",
            slot.as_str()
        ));
        assert_eq!(parse(&text).unwrap().ui.contributions[0].slot, slot);
    }
    assert_eq!(Slot::ALL.len(), 9);
    let bad = with("ui:\n  contributions:\n    - { slot: sidebar, title: T, element: ui/x.html }\n");
    assert_problem(&bad, "unknown slot `sidebar`");
    assert_problem(&bad, "known slots are menu, config-section");
    assert_problem(&with("ui:\n  contributions:\n    - { title: T, element: ui/x.html }\n"), "`slot` is required");
    assert_problem(&with("ui:\n  contributions:\n    - { slot: menu, element: ui/x.html }\n"), "`title` is required");
    assert_problem(&with("ui:\n  contributions:\n    - { slot: menu, title: T }\n"), "`element` is required");
}

#[test]
fn package_paths_cannot_escape_the_package() {
    for bad in ["../x.html", "/etc/passwd", "ui/../../x.html", "ui\\\\x.html", "."] {
        let text = with(&format!("ui:\n  contributions:\n    - {{ slot: menu, title: T, element: \"{bad}\" }}\n"));
        assert_problem(&text, "must be a relative path inside the package");
    }
    assert_problem(&with("ui:\n  config_section: { schema: ../schema.json }\n"), "ui.config_section.schema");
    assert_problem(&with("ui:\n  config_tab: { title: T, schema: /abs.json }\n"), "ui.config_tab.schema");
    assert!(parse(&with("ui:\n  contributions:\n    - { slot: menu, title: T, element: ./ui/x.html }\n")).is_ok());
}

#[test]
fn config_extensions_need_a_section_and_a_schema_object() {
    assert_problem(&with("ui:\n  config_extensions: [{ schema: { type: object } }]\n"), "`section` is required");
    assert_problem(&with("ui:\n  config_extensions: [{ section: proxy }]\n"), "`schema` is required");
    assert_problem(
        &with("ui:\n  config_extensions: [{ section: proxy, schema: nope }]\n"),
        "must be a JSON Schema object",
    );
    assert_problem(
        &with("ui:\n  config_extensions: [{ section: Proxy!, schema: { type: object } }]\n"),
        "not a config section id",
    );
}

#[test]
fn unknown_config_sections_are_reported_against_the_hosts_list() {
    let manifest = parse(&with(
        "ui:\n  config_extensions:\n    - { section: webhooks, schema: { type: object } }\n    - { section: nonsense, schema: { type: object } }\n",
    ))
    .unwrap();
    assert_eq!(manifest.unknown_config_sections(&["webhooks", "proxy", "schedules"]), ["nonsense"]);
    assert_eq!(manifest.unknown_config_sections(&["webhooks", "nonsense"]).len(), 0);
}

#[test]
fn title_keys_need_locales() {
    let no_locales =
        with("ui:\n  contributions:\n    - { slot: menu, title: T, title_key: menu.title, element: ui/x.html }\n");
    assert_problem(&no_locales, "title_key `menu.title` needs `ui.locales`");
    let with_locales = with(
        "ui:\n  locales: [en]\n  contributions:\n    - { slot: menu, title: T, title_key: menu.title, element: ui/x.html }\n",
    );
    assert!(parse(&with_locales).is_ok());
    assert_problem(
        &with("ui:\n  config_tab: { title_key: tab.title, schema: t.json }\n"),
        "title_key `tab.title` needs",
    );
    assert_problem(&with("ui:\n  locales: [en, \"../x\"]\n"), "ui.locales `../x`");
}

#[test]
fn scope_groups_must_compile_and_item_types_must_exist() {
    assert_problem(&with("scope:\n  groups: [\"(unclosed\"]\n"), "scope.groups pattern `(unclosed`");
    assert_problem(&with("scope:\n  item_types: [live, channel]\n"), "scope.item_types `channel`");
    let manifest =
        parse(&with("scope:\n  item_types: [live, Video, series-info, live_hls, movie, live, catchup]\n")).unwrap();
    assert_eq!(
        manifest.scope.item_types,
        [
            PlaylistItemType::Live,
            PlaylistItemType::Video,
            PlaylistItemType::SeriesInfo,
            PlaylistItemType::LiveHls,
            PlaylistItemType::Catchup
        ],
        "`movie` is Video, duplicates collapse"
    );
}

#[test]
fn provides_output_format_is_a_kebab_id() {
    assert_problem(
        &with("hooks: [output.render]\nprovides: { output_format: \"My Format\" }\n"),
        "provides.output_format `My Format` is invalid",
    );
}

#[test]
fn resources_are_non_zero_and_fail_mode_is_known() {
    assert_problem(&with("resources: { max_memory_mb: 0 }\n"), "max_memory_mb must not be zero");
    assert_problem(&with("resources: { max_exec_ms: 0 }\n"), "max_exec_ms must not be zero");
    assert_problem(&with("resources: { max_instances: 0 }\n"), "max_instances must not be zero");
    assert_problem(
        &with("resources: { fail_mode: explode }\n"),
        "fail_mode `explode` is not one of continue, abort-target",
    );
    assert_eq!(
        parse(&with("resources: { fail_mode: abort-target }\n")).unwrap().resources.fail_mode,
        FailMode::AbortTarget
    );
    assert_eq!(parse(&with("resources: { fail_mode: continue }\n")).unwrap().resources.fail_mode, FailMode::Continue);
}

#[test]
fn every_problem_is_reported_in_one_error() {
    let text = "id: Bad\nname: \"\"\nversion: x\napi_version: 3\nhooks: [after-filter, tasks.run, nope]\n\
                events: [nope.event]\ntasks: [{ id: A }]\nscope: { item_types: [bogus] }\n\
                resources: { fail_mode: x }\n";
    let found = problems(text);
    assert!(found.len() >= 12, "{found:#?}");
    let message = rejected(text);
    assert!(message.starts_with("plugin error: invalid manifest.yml in `demo-plugin`:"), "{message}");
}

#[test]
fn epg_resolve_conflict_is_rejected_as_planned_for_later_release() {
    assert_problem(
        &with("hooks: [epg.resolve-conflict]\n"),
        "hook `epg.resolve-conflict` is not supported yet: planned for a later ABI 1.x release",
    );
}

#[test]
fn pipeline_hooks_is_the_subset_the_host_can_run() {
    let manifest =
        parse(&with("hooks: [playlist.after-sort, playlist.after-filter, playlist.loaded, playlist.after-filter]\n"))
            .unwrap();
    assert_eq!(manifest.pipeline_hooks(), [PluginHook::AfterSort, PluginHook::AfterFilter, PluginHook::PlaylistLoaded]);
    assert_eq!(
        parse(&with("hooks: [rpc.call]\nui:\n  rpc_exports: [{ name: ping, kind: read }]\n"))
            .unwrap()
            .pipeline_hooks()
            .len(),
        0
    );
}

#[test]
fn a_manifest_may_declare_its_category() {
    assert_eq!(parse(MINIMAL).unwrap().category, None, "the category is optional");
    assert_eq!(parse(&with("category: notifier\n")).unwrap().category.as_deref(), Some("notifier"));
    assert_eq!(parse(&with("category: live-tv\n")).unwrap().category.as_deref(), Some("live-tv"));
    // A blank category is the same as none.
    assert_eq!(parse(&with("category: \"  \"\n")).unwrap().category, None);
}

#[test]
fn a_category_that_is_not_an_id_is_refused_and_not_silently_rewritten() {
    for bad in ["Notifier", "live tv", "live_tv", "9lives", "x", &"y".repeat(33)] {
        assert_problem(&with(&format!("category: \"{bad}\"\n")), "`category`");
    }
    assert_problem(&with("category: Notifier\n"), "use 2 to 32 lowercase letters, digits and `-`");
}
