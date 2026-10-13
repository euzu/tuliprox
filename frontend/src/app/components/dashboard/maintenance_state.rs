//! What the maintenance card and dialog derive from the server state.

use shared::model::{InputMaintenance, MaintenanceEntries, MaintenanceStateDto, MAINTENANCE_REASON_MAX_CHARS};

pub fn is_alert(state: &MaintenanceStateDto) -> bool { !state.entries.is_empty() }

/// The first `max` input names, with an ellipsis when there are more.
pub fn preview_names(state: &MaintenanceStateDto, max: usize) -> String {
    let mut names: Vec<&str> = state.entries.keys().map(String::as_str).take(max).collect();
    if state.entries.len() > max {
        names.push("…");
    }
    names.join(", ")
}

/// Applies a loaded state. A load that raced with a newer change event keeps the event's
/// entries and only takes the input list.
pub fn merge_loaded(current: &MaintenanceStateDto, loaded: MaintenanceStateDto, stale: bool) -> MaintenanceStateDto {
    if stale {
        MaintenanceStateDto { entries: current.entries.clone(), inputs: loaded.inputs }
    } else {
        loaded
    }
}

/// The server rejects longer remarks; the dialog says so before sending.
pub fn reason_too_long(reason: &str) -> bool { reason.trim().chars().count() > MAINTENANCE_REASON_MAX_CHARS }

fn normalized(reason: &str) -> Option<String> {
    let reason = reason.trim();
    (!reason.is_empty()).then(|| reason.to_string())
}

/// Puts `name` into the draft, or updates its remark; an existing entry keeps its `since`.
pub fn add_to_draft(draft: &MaintenanceEntries, name: &str, reason: &str, now: i64) -> MaintenanceEntries {
    let mut next = draft.clone();
    let reason = normalized(reason);
    next.entry(name.to_string())
        .and_modify(|entry| entry.reason.clone_from(&reason))
        .or_insert(InputMaintenance { since: now, reason });
    next
}

/// Puts every known input that is not in the draft yet into it.
pub fn add_all_to_draft(draft: &MaintenanceEntries, inputs: &[String], reason: &str, now: i64) -> MaintenanceEntries {
    let mut next = draft.clone();
    let reason = normalized(reason);
    for name in inputs {
        next.entry(name.clone()).or_insert_with(|| InputMaintenance { since: now, reason: reason.clone() });
    }
    next
}

/// What Save has to send to turn the saved state into the draft.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PendingChanges {
    /// New inputs and changed remarks.
    pub puts: Vec<(String, Option<String>)>,
    pub deletes: Vec<String>,
}

impl PendingChanges {
    pub fn is_empty(&self) -> bool { self.puts.is_empty() && self.deletes.is_empty() }
}

pub fn pending_changes(saved: &MaintenanceEntries, draft: &MaintenanceEntries) -> PendingChanges {
    let puts = draft
        .iter()
        .filter(|(name, entry)| saved.get(*name).is_none_or(|current| current.reason != entry.reason))
        .map(|(name, entry)| (name.clone(), entry.reason.clone()))
        .collect();
    let deletes = saved.keys().filter(|name| !draft.contains_key(*name)).cloned().collect();
    PendingChanges { puts, deletes }
}

/// How long an input has been in maintenance, in the two largest units.
pub fn relative_duration(since: i64, now: i64) -> String {
    let secs = (now - since).max(0);
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "<1m".to_string(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{InputMaintenance, MaintenanceEntries, MaintenanceStateDto};

    fn state(in_maintenance: &[&str], inputs: &[&str]) -> MaintenanceStateDto {
        MaintenanceStateDto {
            entries: in_maintenance
                .iter()
                .map(|name| ((*name).to_string(), InputMaintenance { since: 1, reason: None }))
                .collect(),
            inputs: inputs.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn card_is_alert_only_with_entries() {
        assert!(!is_alert(&state(&[], &["a"])));
        assert!(is_alert(&state(&["a"], &["a"])));
    }

    #[test]
    fn preview_names_are_truncated() {
        let s = state(&["a", "b", "c", "d"], &["a", "b", "c", "d"]);
        assert_eq!(preview_names(&s, 3), "a, b, c, …");
        assert_eq!(preview_names(&state(&["a"], &["a"]), 3), "a");
    }

    #[test]
    fn a_stale_load_keeps_newer_entries_but_takes_the_input_list() {
        let current = state(&["a"], &["a"]);
        let loaded = state(&[], &["a", "b"]);
        let merged = merge_loaded(&current, loaded.clone(), true);
        assert_eq!(merged.entries, current.entries, "entries from a newer event win");
        assert_eq!(merged.inputs, loaded.inputs);
        assert_eq!(merge_loaded(&current, loaded.clone(), false), loaded);
    }

    #[test]
    fn reason_limit_is_checked_in_characters() {
        let limit = shared::model::MAINTENANCE_REASON_MAX_CHARS;
        assert!(!reason_too_long(&"ä".repeat(limit)));
        assert!(reason_too_long(&"x".repeat(limit + 1)));
        assert!(!reason_too_long(&format!("  {}  ", "x".repeat(limit))), "surrounding whitespace is trimmed");
    }

    fn entries(items: &[(&str, i64, Option<&str>)]) -> MaintenanceEntries {
        items
            .iter()
            .map(|(name, since, reason)| {
                ((*name).to_string(), InputMaintenance { since: *since, reason: reason.map(ToString::to_string) })
            })
            .collect()
    }

    #[test]
    fn adding_to_the_draft_keeps_since_of_an_existing_entry() {
        let draft = entries(&[("a", 10, Some("old"))]);
        let updated = add_to_draft(&draft, "a", "  new ", 99);
        assert_eq!(updated, entries(&[("a", 10, Some("new"))]));
        let added = add_to_draft(&draft, "b", " ", 99);
        assert_eq!(added, entries(&[("a", 10, Some("old")), ("b", 99, None)]));
    }

    #[test]
    fn pending_changes_lists_puts_for_new_or_changed_and_deletes_for_removed() {
        let saved = entries(&[("a", 1, None), ("b", 1, Some("x")), ("c", 1, None)]);
        let draft = entries(&[("a", 1, None), ("b", 5, Some("y")), ("d", 9, Some("z"))]);
        let changes = pending_changes(&saved, &draft);
        assert_eq!(
            changes.puts,
            vec![("b".to_string(), Some("y".to_string())), ("d".to_string(), Some("z".to_string()))]
        );
        assert_eq!(changes.deletes, vec!["c".to_string()]);
        assert!(!changes.is_empty());
        assert!(pending_changes(&saved, &saved).is_empty());
    }

    #[test]
    fn draft_bulk_helpers_cover_all_inputs() {
        let s = state(&["a"], &["a", "b"]);
        let all = add_all_to_draft(&s.entries, &s.inputs, "outage", 7);
        assert_eq!(all.len(), 2);
        assert_eq!(all["a"].since, 1, "existing entries keep their since");
        assert_eq!(all["b"], InputMaintenance { since: 7, reason: Some("outage".into()) });
    }

    #[test]
    fn relative_duration_is_compact() {
        assert_eq!(relative_duration(1_000, 1_000 + 59), "<1m");
        assert_eq!(relative_duration(1_000, 1_000 + 300), "5m");
        assert_eq!(relative_duration(1_000, 1_000 + 3 * 3600 + 120), "3h 2m");
        assert_eq!(relative_duration(1_000, 1_000 + 2 * 86_400 + 3600), "2d 1h");
        assert_eq!(relative_duration(1_000, 500), "<1m", "clock skew never shows a negative duration");
    }
}
