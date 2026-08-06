//! DVR library + quota view.
//!
//! The view keeps recordings separate from other media, displays private and
//! shared quota, and gates delete/edit controls by `recording.write` plus the
//! per-task ownership policy.

use shared::model::recording::{RecordingOwner, RecordingVisibility};

/// Permission gate: should the DVR navigation entry show?
/// True when the principal has `recording.read`.
pub fn can_show_dvr_nav(has_recording_read: bool) -> bool { has_recording_read }

/// Permission gate: can this principal edit/delete the given
/// task? Real users may edit their own private tasks with
/// `recording.write`. Administrators may edit/delete any
/// visible task (private, shared, or `LegacyAdmin`).
///
/// `is_admin_role` is true when the principal's roles include
/// the built-in administrator role. `is_owner` is true when
/// the principal is the immutable `UserId` owner of a private
/// task.
pub fn can_mutate_task(has_recording_write: bool, is_admin_role: bool, is_owner: bool) -> bool {
    if !has_recording_write {
        return false;
    }
    is_admin_role || is_owner
}

/// A task is visible in the recording library unless it is marked
/// `Deleting` (read via the DTO's `deleting_previous_state` flag)
/// or has no recording metadata — i.e. it is a generic download
/// rather than a recording.
pub fn is_visible_recording_task(
    owner: Option<&RecordingOwner>,
    visibility: Option<&RecordingVisibility>,
    deleting_previous_state: bool,
) -> bool {
    if deleting_previous_state {
        return false;
    }
    owner.is_some() && visibility.is_some()
}

/// Format a byte count for the human-readable quota display.
/// The frontend already has a human-readable helper
/// (`humanize_bytes`); this is a small wrapper that returns
/// `unlimited` for `None` so the view does not need to special-
/// case the absence of a configured limit.
pub fn quota_line(used: u64, limit: Option<u64>) -> String {
    match limit {
        Some(limit) => format!("{used} / {limit} bytes"),
        None => format!("{used} bytes (unlimited)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::UserId;

    #[test]
    fn dvr_nav_only_with_recording_read() {
        assert!(!can_show_dvr_nav(false));
        assert!(can_show_dvr_nav(true));
    }

    #[test]
    fn mutate_requires_write_and_owner_or_admin() {
        // No write → no
        assert!(!can_mutate_task(false, true, true));
        // Write + admin → yes
        assert!(can_mutate_task(true, true, false));
        // Write + owner → yes
        assert!(can_mutate_task(true, false, true));
        // Write only, neither owner nor admin → no
        assert!(!can_mutate_task(true, false, false));
    }

    #[test]
    fn visible_recording_requires_owner_and_visibility() {
        let owner = RecordingOwner::User(UserId::from("web:alice"));
        let visibility = RecordingVisibility::Private;
        assert!(is_visible_recording_task(Some(&owner), Some(&visibility), false));
        // deleting → invisible
        assert!(!is_visible_recording_task(Some(&owner), Some(&visibility), true));
        // generic download (no recording metadata) → invisible
        assert!(!is_visible_recording_task(None, None, false));
    }

    #[test]
    fn quota_line_handles_unlimited() {
        assert_eq!(quota_line(100, Some(1000)), "100 / 1000 bytes");
        assert_eq!(quota_line(100, None), "100 bytes (unlimited)");
    }
}
