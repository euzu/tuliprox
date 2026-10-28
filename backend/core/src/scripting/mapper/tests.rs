use super::*;
use shared::model::{PatternTemplate, PlaylistItemHeader, PlaylistItemType, TemplateValue};
use std::sync::Arc;

fn item(name: &str) -> PlaylistItem {
    PlaylistItem { header: PlaylistItemHeader { name: name.intern(), ..Default::default() } }
}

fn script(source: &str) -> MappingScript { MappingScript::compile(source).expect("script should compile") }

fn run(source: &str, item: PlaylistItem) -> MappingScriptOutcome {
    script(source).eval(item, MappingScriptContext::default())
}

fn template(name: &str, value: &str) -> PatternTemplate {
    PatternTemplate {
        name: name.to_string(),
        value: TemplateValue::Single(value.to_string()),
        placeholder: format!("!{name}!"),
    }
}

#[test]
fn field_assignment_tracks_changed_fields() {
    let outcome = run(r#"item.group = "News";"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "News");
    assert_eq!(outcome.changed_fields, vec!["group".to_string()]);
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
}

#[test]
fn caption_writes_both_name_and_title_like_the_dsl() {
    let outcome = run(r#"item.caption = "Renamed";"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.name.as_ref(), "Renamed");
    assert_eq!(mapped.header.title.as_ref(), "Renamed");
}

#[test]
fn chno_is_numeric_and_accepts_a_string() {
    let numeric = run("item.chno = 42;", item("Channel"));
    let mapped = numeric.item.expect("item survives the script");
    assert_eq!(mapped.header.chno, 42);

    let textual = run(r#"item.chno = "7";"#, item("Channel"));
    let mapped = textual.item.expect("item survives the script");
    assert_eq!(mapped.header.chno, 7);
}

#[test]
fn indexer_uses_the_same_field_aliases_as_the_dsl() {
    let outcome = run(r#"item["epg_id"] = "epg-1"; item["Caption"] = "Alias";"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.epg_channel_id.as_deref(), Some("epg-1"));
    assert_eq!(mapped.header.name.as_ref(), "Alias");
}

#[test]
fn reading_an_unknown_field_yields_unit_instead_of_failing() {
    let outcome = run(r#"if item["nope"] == () { item.group = "fallback"; }"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "fallback");
}

#[test]
fn writing_an_unknown_field_fails_with_a_diagnostic() {
    let outcome = run(r#"item["nope"] = "x";"#, item("Channel"));

    assert!(outcome.item.is_some(), "the item must survive a failed assignment");
    assert_eq!(outcome.diagnostics.len(), 1);
    assert!(outcome.diagnostics[0].contains("unknown field 'nope'"), "{:?}", outcome.diagnostics);
}

#[test]
fn read_only_fields_reject_assignment() {
    let outcome = run(r#"item.input = "provider";"#, item("Channel"));

    assert!(outcome.item.is_some(), "the item must survive a failed assignment");
    assert_eq!(outcome.diagnostics.len(), 1);
    assert!(outcome.diagnostics[0].contains("read-only"), "{:?}", outcome.diagnostics);
}

#[test]
fn genre_is_writable_on_an_item_that_carries_it() {
    // `MAPPER_FIELDS` is the legacy MapperOperation allow-list and omits genre, while the DSL
    // grammar accepts @genre. The script vocabulary must follow the DSL.
    let mut channel = item("Movie");
    channel.header.item_type = PlaylistItemType::Video;
    let outcome = run(r#"item.genre = "Action";"#, channel);

    let mapped = outcome.item.expect("item survives the script");
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
    assert_eq!(outcome.changed_fields, vec!["genre".to_string()]);
    assert_eq!(
        mapped
            .header
            .additional_properties
            .as_ref()
            .and_then(shared::model::StreamProperties::genre)
            .map(std::sync::Arc::as_ref),
        Some("Action")
    );
}

#[test]
fn a_field_the_item_cannot_carry_is_reported_without_aborting_the_script() {
    // A live channel has no genre slot. The DSL logs and continues; the script must not lose the
    // statements that follow.
    let outcome = run(r#"item.genre = "Action"; item.group = "afterwards";"#, item("Live"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "afterwards", "statements after the refused field must still run");
    assert_eq!(outcome.diagnostics.len(), 1);
    assert!(outcome.diagnostics[0].contains("could not assign genre"), "{:?}", outcome.diagnostics);
}

#[test]
fn regex_exposes_indexed_and_named_captures() {
    let outcome = run(
        r#"
            let m = ctx.regex(item.name, "^(?P<show>.+) - (\\d{4})$");
            if !m.is_empty() {
                item.group = m["show"];
                item.title = m["2"];
            }
        "#,
        item("Show - 2024"),
    );

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "Show");
    assert_eq!(mapped.header.title.as_ref(), "2024");
}

#[test]
fn regex_honours_match_as_ascii() {
    let templates = None;
    let context = MappingScriptContext::new(templates, true);
    let outcome = script(r#"item.group = ctx.regex(item.name, "(Cinema)")["1"];"#).eval(item("Cinéma"), context);

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "Cinema");
}

#[test]
fn regex_without_a_match_returns_an_empty_map() {
    let outcome = run(r#"if ctx.regex(item.name, "^missing$").is_empty() { item.group = "none"; }"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "none");
}

#[test]
fn template_reads_the_mapping_template_list() {
    let templates: Arc<[PatternTemplate]> = Arc::from(vec![template("quality", "FHD")]);
    let context = MappingScriptContext::new(Some(templates), false);

    let outcome = script(r#"item.group = ctx.template("quality");"#).eval(item("Channel"), context);

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "FHD");
}

#[test]
fn unknown_template_yields_unit() {
    let outcome = run(r#"if ctx.template("missing") == () { item.group = "none"; }"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "none");
}

#[test]
fn favourite_adds_an_alias_row_with_a_derived_uuid() {
    let outcome = run(r#"ctx.favourite(item, "Favourites");"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "");
    assert_eq!(outcome.virtual_items.len(), 1);
    let alias = &outcome.virtual_items[0];
    assert_eq!(alias.header.group.as_ref(), "Favourites");
    assert_ne!(alias.header.uuid, mapped.header.uuid, "alias row must get a distinct uuid");
}

#[test]
fn reassigning_the_item_discards_the_channel_instead_of_hiding_it() {
    let outcome = run("item = 42;", item("Channel"));

    assert!(outcome.item.is_none());
    assert_eq!(outcome.diagnostics.len(), 1);
    assert!(outcome.diagnostics[0].contains("replaced the reserved variable 'item'"), "{:?}", outcome.diagnostics);
}

#[test]
fn shadowing_the_item_keeps_the_channel() {
    // `let item = item.name;` reads the name and shadows the binding. Rhai keeps both bindings,
    // so the host must recover the real item instead of discarding the channel.
    let outcome = run(r#"let item = item.name; item = "ignored";"#, item("Channel"));

    let mapped = outcome.item.expect("a shadowed item must not cost the channel");
    assert_eq!(mapped.header.name.as_ref(), "Channel");
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
}

#[test]
fn shadowing_the_context_keeps_templates_and_aliases() {
    let templates: Arc<[PatternTemplate]> = Arc::from(vec![template("quality", "FHD")]);
    let context = MappingScriptContext::new(Some(templates), false);

    let outcome = script(r#"let ctx = 1; item.group = "kept";"#).eval(item("Channel"), context);

    let mapped = outcome.item.expect("the item survives");
    assert_eq!(mapped.header.group.as_ref(), "kept");
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
}

#[test]
fn property_and_index_forms_resolve_any_field_casing() {
    // Rhai falls back to the registered indexer for a property it has no accessor for, so a
    // differently cased property must behave exactly like the index form and like the DSL.
    for source in [r#"item.NAME = "Alias";"#, r#"item["name"] = "Alias";"#, r#"item.Name = "Alias";"#] {
        let outcome = run(source, item("Channel"));
        let mapped = outcome.item.expect("item survives the script");
        assert_eq!(mapped.header.name.as_ref(), "Alias", "failed for {source}");
        assert!(outcome.diagnostics.is_empty(), "{source}: {:?}", outcome.diagnostics);
    }
}

#[test]
fn reading_an_unknown_property_yields_unit_instead_of_failing() {
    let outcome = run(r#"if item.NOPE == () { item.group = "fallback"; }"#, item("Channel"));

    let mapped = outcome.item.expect("item survives the script");
    assert_eq!(mapped.header.group.as_ref(), "fallback");
    assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
}
