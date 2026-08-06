//! Recurring-rule view.
//!
//! The view lists and mutates recurring rules, shows matching limitations, and
//! gates shared controls to administrators.

/// Permission gate: may the principal see the recurring-rule
/// section at all? Any user with `recording.read` can list rules; creation
/// needs `recording.write`.
pub fn can_show_rules_section(has_recording_read: bool) -> bool { has_recording_read }

/// Permission gate: may the principal create new rules? Owners
/// can create private rules; only administrators can create shared
/// rules.
pub fn can_create_rule(has_recording_write: bool) -> bool { has_recording_write }

/// Permission gate: may the principal create a *shared* rule?
/// Administrators with `recording.write` only.
pub fn can_create_shared_rule(has_recording_write: bool, is_admin_role: bool) -> bool {
    has_recording_write && is_admin_role
}

/// Permission gate: may the principal edit this rule?
/// - Private rule: owner with `recording.write`.
/// - Shared rule: administrator with `recording.write`.
pub fn can_edit_rule(has_recording_write: bool, is_admin_role: bool, is_owner: bool, is_shared: bool) -> bool {
    if !has_recording_write {
        return false;
    }
    if is_shared {
        is_admin_role
    } else {
        is_admin_role || is_owner
    }
}

/// Permission gate: may the principal delete this rule?
/// Same matrix as edit.
pub fn can_delete_rule(has_recording_write: bool, is_admin_role: bool, is_owner: bool, is_shared: bool) -> bool {
    can_edit_rule(has_recording_write, is_admin_role, is_owner, is_shared)
}

/// The delete future-policy options exposed to the user. The API
/// requires `future=retain|cancel`; the UI mirrors that with two
/// radio buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteFuture {
    Retain,
    Cancel,
}

impl DeleteFuture {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Retain => "retain",
            Self::Cancel => "cancel",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "retain" => Some(Self::Retain),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// A short, user-facing note for recurring-rule limitations.
/// calls out. The form's text surfaces these alongside the
/// matching-field inputs.
pub fn new_episode_limitations_text() -> &'static str {
    "When the EPG does not publish a stable series id, the rule falls back to the title. \
     Title fallback may record reruns when provider metadata is incomplete."
}

/// DST + timezone explanation for weekly rules. The form's text
/// surfaces this next to the timezone input.
pub fn weekly_timezone_hint_text() -> &'static str {
    "Local wall-clock time. Daylight-saving transitions follow the timezone: \
     ambiguous times (fall-back) pick the earlier instant; nonexistent times \
     (spring-forward) advance to the next valid instant."
}

/// Map a backend reconciliation error to a stable i18n key the
/// form can render. A failed request may have applied a partial
/// change on the server, so the message tells the user what
/// state the system is in.
pub fn reconciliation_error_to_i18n_key(primary: &str, secondary: &str) -> String {
    format!("MESSAGES.RECORDING.PARTIAL_OPERATION/{primary}/{secondary}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_show_rules_section_requires_recording_read() {
        assert!(!can_show_rules_section(false));
        assert!(can_show_rules_section(true));
    }

    #[test]
    fn can_create_rule_requires_recording_write() {
        assert!(!can_create_rule(false));
        assert!(can_create_rule(true));
    }

    #[test]
    fn can_create_shared_rule_requires_admin() {
        assert!(!can_create_shared_rule(false, false));
        assert!(!can_create_shared_rule(false, true));
        assert!(!can_create_shared_rule(true, false));
        assert!(can_create_shared_rule(true, true));
    }

    #[test]
    fn can_edit_private_rule_owner_or_admin() {
        assert!(!can_edit_rule(false, false, true, false));
        assert!(can_edit_rule(true, false, true, false));
        assert!(!can_edit_rule(true, false, false, false));
        assert!(can_edit_rule(true, true, false, false));
    }

    #[test]
    fn can_edit_shared_rule_only_admin() {
        assert!(!can_edit_rule(true, false, true, true));
        assert!(can_edit_rule(true, true, false, true));
    }

    #[test]
    fn can_delete_rule_matches_edit_rule() {
        for (a, b, c, d) in [(false, false, true, false), (true, true, false, true), (true, false, true, false)] {
            assert_eq!(can_edit_rule(a, b, c, d), can_delete_rule(a, b, c, d));
        }
    }

    #[test]
    fn delete_future_round_trip() {
        assert_eq!(DeleteFuture::from_wire("retain"), Some(DeleteFuture::Retain));
        assert_eq!(DeleteFuture::from_wire("cancel"), Some(DeleteFuture::Cancel));
        assert_eq!(DeleteFuture::from_wire("bogus"), None);
        assert_eq!(DeleteFuture::Retain.wire(), "retain");
        assert_eq!(DeleteFuture::Cancel.wire(), "cancel");
    }

    #[test]
    fn new_episode_limitations_text_mentions_title_fallback() {
        assert!(new_episode_limitations_text().contains("title"));
    }

    #[test]
    fn weekly_timezone_hint_text_mentions_dst() {
        assert!(weekly_timezone_hint_text().to_lowercase().contains("daylight"));
    }

    #[test]
    fn reconciliation_error_to_i18n_key_carries_both_labels() {
        let k = reconciliation_error_to_i18n_key("rule", "tombstone");
        assert!(k.contains("rule"));
        assert!(k.contains("tombstone"));
    }
}
