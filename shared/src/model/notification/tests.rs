use super::{registry, EventId, EventPattern, EventSubscription, Severity, LEGACY_ALIASES};

#[test]
fn every_legacy_msgkind_wire_name_still_resolves() {
    // Load-bearing for existing config.yml files: each of these was a
    // valid `notify_on` entry before the open-world refactor.
    for legacy in [
        "info",
        "stats",
        "error",
        "watch",
        "disk_alert",
        "recording_started",
        "recording_completed",
        "recording_failed",
    ] {
        assert!(EventId::from_wire(legacy).is_some(), "legacy wire name {legacy} no longer resolves");
    }
}

#[test]
fn legacy_alias_targets_are_all_registered() {
    for (legacy, id) in LEGACY_ALIASES {
        assert!(registry::describe(*id).is_some(), "alias {legacy} points at unregistered id {id}");
    }
}

#[test]
fn canonical_ids_resolve_and_round_trip() {
    for descriptor in registry::ALL {
        let resolved = EventId::from_wire(descriptor.id.as_str());
        assert_eq!(resolved, Some(descriptor.id), "id {} did not round-trip", descriptor.id);
    }
}

#[test]
fn unknown_wire_name_does_not_resolve() {
    assert_eq!(EventId::from_wire("recording.definitely_not_a_real_event"), None);
}

#[test]
fn recording_template_filenames_match_the_legacy_names() {
    // The legacy on-disk names were `telegram_recording_completed.templ`
    // and friends. The canonical dotted ids produce the same stems, so
    // existing template files keep being discovered.
    assert_eq!(registry::RECORDING_COMPLETED.template_filename("telegram"), "telegram_recording_completed.templ");
    assert_eq!(registry::RECORDING_STARTED.template_filename("discord"), "discord_recording_started.templ");
    assert_eq!(registry::RECORDING_FAILED.template_filename("rest"), "rest_recording_failed.templ");
}

#[test]
fn domain_is_the_part_before_the_first_dot() {
    assert_eq!(registry::RECORDING_COMPLETED.domain(), "recording");
    assert_eq!(registry::SYSTEM_DISK_ALERT.domain(), "system");
    assert_eq!(registry::UNKNOWN.domain(), "unknown");
}

#[test]
fn star_matches_every_registered_event() {
    let sub = EventSubscription::parse(["*"]);
    for descriptor in registry::ALL {
        assert!(sub.matches(descriptor.id), "`*` failed to match {}", descriptor.id);
    }
}

#[test]
fn prefix_pattern_matches_the_domain_and_nothing_else() {
    let sub = EventSubscription::parse(["recording.*"]);
    assert!(sub.matches(registry::RECORDING_COMPLETED));
    assert!(sub.matches(registry::RECORDING_FAILED));
    assert!(!sub.matches(registry::SYSTEM_INFO));
    assert!(!sub.matches(registry::PLAYLIST_UPDATE_COMPLETED));
}

#[test]
fn prefix_pattern_requires_at_least_one_further_segment() {
    // `recording.*` must not match a hypothetical bare `recording`.
    assert!(!EventPattern::parse("recording.*").matches(EventId::new("recording")));
}

#[test]
fn exact_pattern_matches_only_itself() {
    let sub = EventSubscription::parse(["recording.completed"]);
    assert!(sub.matches(registry::RECORDING_COMPLETED));
    assert!(!sub.matches(registry::RECORDING_STARTED));
}

#[test]
fn interior_wildcard_matches_exactly_one_segment() {
    let pattern = EventPattern::parse("provider.*.expired");
    assert!(pattern.matches(registry::PROVIDER_ACCOUNT_EXPIRED));
    // One segment, not several.
    assert!(!pattern.matches(EventId::new("provider.a.b.expired")));
    // And not zero.
    assert!(!pattern.matches(EventId::new("provider.expired")));
}

#[test]
fn negation_removes_from_a_wider_match() {
    let sub = EventSubscription::parse(["*", "!system.info"]);
    assert!(sub.matches(registry::SYSTEM_ERROR));
    assert!(!sub.matches(registry::SYSTEM_INFO), "negated pattern did not exclude");
}

#[test]
fn negation_wins_regardless_of_order() {
    let before = EventSubscription::parse(["!recording.started", "recording.*"]);
    let after = EventSubscription::parse(["recording.*", "!recording.started"]);
    assert!(!before.matches(registry::RECORDING_STARTED));
    assert!(!after.matches(registry::RECORDING_STARTED));
    assert!(before.matches(registry::RECORDING_COMPLETED));
    assert!(after.matches(registry::RECORDING_COMPLETED));
}

#[test]
fn a_subscription_of_only_negations_matches_nothing() {
    // No positive pattern means nothing was opted in to.
    let sub = EventSubscription::parse(["!system.info"]);
    assert!(!sub.matches(registry::SYSTEM_ERROR));
    assert!(!sub.matches(registry::SYSTEM_INFO));
}

#[test]
fn empty_subscription_matches_nothing() {
    let sub = EventSubscription::parse(Vec::<String>::new());
    assert!(sub.is_empty());
    assert!(!sub.matches(registry::SYSTEM_INFO));
}

#[test]
fn blank_pattern_matches_nothing_rather_than_everything() {
    // A stray empty line in config must not silently subscribe to all.
    let sub = EventSubscription::parse(["  "]);
    assert!(!sub.matches(registry::SYSTEM_INFO));
}

#[test]
fn unmatched_patterns_flags_typos_only() {
    let sub = EventSubscription::parse(["recording.*", "recroding.completed", "*"]);
    assert_eq!(sub.unmatched_patterns(), vec!["recroding.completed"]);
}

#[test]
fn severity_orders_from_info_up_to_critical() {
    assert!(Severity::Info < Severity::Warn);
    assert!(Severity::Warn < Severity::Error);
    assert!(Severity::Error < Severity::Critical);
}

#[test]
fn severity_wire_names_round_trip() {
    for severity in [Severity::Info, Severity::Warn, Severity::Error, Severity::Critical] {
        assert_eq!(Severity::from_wire(severity.wire_name()), Some(severity));
    }
}

#[test]
fn unknown_event_id_deserializes_to_unknown_instead_of_failing() {
    // A newer build's outbox entry must not poison the whole file when
    // read back by an older one.
    let id: EventId = serde_json::from_str("\"some.future.event\"").expect("must not fail");
    assert_eq!(id, registry::UNKNOWN);
}

#[test]
fn event_id_serializes_as_its_wire_string() {
    let json = serde_json::to_string(&registry::RECORDING_COMPLETED).expect("serialize");
    assert_eq!(json, "\"recording.completed\"");
}

/// The docs carry a table of every event id. A registry entry that never
/// reaches the table is undiscoverable, and a table row for an id that no
/// longer exists is a lie - so the two are checked against each other
/// rather than maintained by hand.
#[test]
fn every_registered_event_appears_in_the_docs_table() {
    const DOCS: &str = include_str!("../../../../docs/src/configuration/config.md");
    let table = DOCS
        .split_once("<!-- BEGIN GENERATED EVENT TABLE -->")
        .and_then(|(_, rest)| rest.split_once("<!-- END GENERATED EVENT TABLE -->"))
        .map(|(table, _)| table)
        .expect("docs must contain the generated event table markers");

    for descriptor in registry::ALL {
        let row = format!("| `{}` | {} | {} |", descriptor.id, descriptor.severity, descriptor.description);
        assert!(
            table.contains(&row),
            "docs/src/configuration/config.md is missing this row - add it inside the generated table:\n{row}"
        );
    }

    // And nothing in the table that the registry no longer knows about.
    for line in table.lines().filter(|line| line.trim_start().starts_with("| `")) {
        let id = line.trim_start().trim_start_matches("| `").split('`').next().unwrap_or_default();
        assert!(
            EventId::from_wire(id).is_some(),
            "docs list `{id}`, which is not a registered event - remove the row or register the event"
        );
    }
}

#[test]
fn registry_has_no_duplicate_ids() {
    let mut seen = std::collections::HashSet::new();
    for descriptor in registry::ALL {
        assert!(seen.insert(descriptor.id), "duplicate registry entry for {}", descriptor.id);
    }
}
