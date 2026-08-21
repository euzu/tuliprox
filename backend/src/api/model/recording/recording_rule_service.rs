//! Rule service validation.
//!
//! Rule mutations enforce owner/private/shared authorization, validate
//! matching fields and parse the `future=retain|cancel` deletion policy.


use shared::model::recording_rule::{RecordingRule, RuleVisibility};
use shared::model::UserId;

/// The future-occurrence policy on rule deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteFuture {
    Retain,
    Cancel,
}

impl DeleteFuture {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "retain" => Ok(Self::Retain),
            "cancel" => Ok(Self::Cancel),
            _ => Err("recording_rule_invalid_future"),
        }
    }
}

/// Stable service-layer errors for the rule service. The HTTP
/// handler maps each variant to a wire code; the frontend maps
/// the codes to localized messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleServiceError {
    /// The authenticated principal lacks the recording.write
    /// permission.
    Forbidden,
    /// A non-administrator tried to create, delete, or manage a shared rule.
    SharedManagementNotAdministrator,
    /// The supplied rule failed structural validation (see
    /// `validate_rule`).
    InvalidRule,
    /// The supplied delete policy was not `retain` or `cancel`.
    InvalidFuture,
    /// Rule id missing or malformed.
    UnknownRule,
    /// The owner id on the rule did not match the authenticated
    /// principal, and the principal is not an administrator.
    NotOwner,
    /// Persistence failed; the in-memory state was kept unchanged.
    PersistenceFailed,
    /// A cross-store second step failed. The HTTP handler returns
    /// this with a reconciliation status the operator can use to
    /// decide whether to retry.
    PartialOperation { primary: String, secondary: String },
    /// The rule uses a feature the server knows about but does not
    /// currently implement. The stable code names the feature so the
    /// frontend can render a localized, feature-specific message.
    Unsupported { feature: &'static str },
}

impl RuleServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Forbidden => "recording_rule_forbidden",
            Self::SharedManagementNotAdministrator => "recording_shared_not_administrator",
            Self::InvalidRule => "recording_rule_invalid",
            Self::InvalidFuture => "recording_rule_invalid_future",
            Self::UnknownRule => "recording_rule_unknown",
            Self::NotOwner => "recording_rule_not_owner",
            Self::PersistenceFailed => "recording_persistence_failed",
            Self::PartialOperation { .. } => "recording_rule_partial_operation",
            Self::Unsupported { feature } => match *feature {
                "new_episode_rule" => "recording_rule_new_episode_unsupported",
                _ => "recording_rule_unsupported",
            },
        }
    }
}

/// Pure: validate the structural shape of a rule. Source id,
/// matching fields, timezone, local time, duration and padding.
pub fn validate_rule(rule: &RecordingRule) -> Result<(), RuleServiceError> {
    rule.validate().map_err(|_| RuleServiceError::InvalidRule)?;
    if matches!(
        rule.body,
        shared::model::recording_rule::RuleBody::WeeklyTimeslot { duration_secs, .. }
            if i64::try_from(duration_secs).is_err()
    ) {
        return Err(RuleServiceError::InvalidRule);
    }
    if rule.pre_roll_secs > 15 * 60 {
        return Err(RuleServiceError::InvalidRule);
    }
    if rule.post_roll_secs > 30 * 60 {
        return Err(RuleServiceError::InvalidRule);
    }
    // `RuleBody::NewEpisode` is parsed and persisted, but the scheduler
    // has no EPG horizon to match it against, so the rule would sit
    // inert forever. Refuse it at the edge instead of letting an
    // operator create a rule that never fires.
    if matches!(rule.body, shared::model::recording_rule::RuleBody::NewEpisode { .. }) {
        return Err(RuleServiceError::Unsupported { feature: "new_episode_rule" });
    }
    Ok(())
}

/// The principal / owner / admin decision for any rule mutation:
/// - read: any user with `recording.read`.
/// - create / edit / delete private rule: any user with
///   `recording.write`. Owner must be the principal unless
///   the principal is an administrator.
/// - create / edit / delete shared rule: administrator with
///   `recording.write`.
pub fn authorize_rule_action(
    has_recording_write: bool,
    is_admin_role: bool,
    principal_id: &UserId,
    rule: &RecordingRule,
) -> Result<(), RuleServiceError> {
    if !has_recording_write {
        return Err(RuleServiceError::Forbidden);
    }
    match rule.visibility {
        RuleVisibility::Shared => {
            if !is_admin_role {
                return Err(RuleServiceError::SharedManagementNotAdministrator);
            }
        }
        RuleVisibility::Private => {
            if !is_admin_role && &rule.owner_id != principal_id {
                return Err(RuleServiceError::NotOwner);
            }
        }
    }
    Ok(())
}

/// Validate a delete request. The `future` parameter is required
/// and accepts only `retain` or `cancel`.
pub fn validate_delete(future: Option<&str>) -> Result<DeleteFuture, RuleServiceError> {
    let raw = future.ok_or(RuleServiceError::InvalidFuture)?;
    DeleteFuture::parse(raw).map_err(|_| RuleServiceError::InvalidFuture)
}

/// Per-task policy for the `cancel` delete option. Only future
/// inactive occurrences are cancellable; active recordings are not.
/// The caller filters tasks by scheduled interval.
pub fn cancel_targets_task(task_active: bool, task_editable: bool) -> bool {
    // Active recordings are never auto-cancelled by rule deletion.
    // Editable tasks (upcoming states) are eligible.
    !task_active && task_editable
}

/// Pure: the per-task policy for the `retain` delete option.
/// Existing tasks keep their `rule_id` so reconciliation can still
/// group them. The caller does not modify the task's metadata; it
/// only sets the rule's `enabled = false`.
pub fn retain_targets_task(_task_uuid: &str) -> bool { true }

/// A summary the HTTP handler can serialize when the cross-store
/// second step fails. The response carries a partial-operation
/// status so the client can show the user what state the system
/// is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialOperationStatus {
    pub primary: String,
    pub secondary: String,
}

impl From<&RuleServiceError> for Option<PartialOperationStatus> {
    fn from(err: &RuleServiceError) -> Self {
        if let RuleServiceError::PartialOperation { primary, secondary } = err {
            Some(PartialOperationStatus { primary: primary.clone(), secondary: secondary.clone() })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording_rule::{RuleBody, RuleSource};
    use shared::model::UserId;

    fn user() -> UserId { UserId::from("web:alice") }
    fn other() -> UserId { UserId::from("web:bob") }
    fn admin() -> UserId { UserId::from("builtin:admin") }

    fn private_rule(owner: UserId) -> RecordingRule {
        RecordingRule {
            id: "r1".into(),
            owner_id: owner,
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("tgt", "virt", "input"),
            channel_id: None,
            body: RuleBody::NewEpisode {
                series_id: Some("series-1".into()),
                title_pattern: None,
                exclude_repeat: true,
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn shared_rule() -> RecordingRule {
        let mut r = private_rule(admin());
        r.visibility = RuleVisibility::Shared;
        r
    }

    #[test]
    fn validate_rule_rejects_invalid_source() {
        let mut r = private_rule(user());
        r.source = RuleSource::new("", "virt", "input");
        assert_eq!(validate_rule(&r), Err(RuleServiceError::InvalidRule));
    }

    #[test]
    fn validate_rule_rejects_excessive_padding() {
        let mut r = private_rule(user());
        r.pre_roll_secs = 16 * 60;
        assert_eq!(validate_rule(&r), Err(RuleServiceError::InvalidRule));
        let mut r = private_rule(user());
        r.post_roll_secs = 31 * 60;
        assert_eq!(validate_rule(&r), Err(RuleServiceError::InvalidRule));
    }

    #[test]
    fn validate_rule_rejects_duration_larger_than_i64() {
        let mut r = private_rule(user());
        r.body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".to_string(),
            duration_secs: u64::MAX,
            timezone: "UTC".to_string(),
        };

        assert_eq!(validate_rule(&r), Err(RuleServiceError::InvalidRule));
    }

    #[test]
    fn validate_rule_accepts_within_bounds() {
        // The body is incidental to this assertion; the helper builds
        // a `NewEpisode` body which `validate_rule` now refuses for
        // unrelated reasons. Swap to `WeeklyTimeslot` so the padding
        // bounds are what gets tested.
        let mut r = private_rule(user());
        r.body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".to_string(),
            duration_secs: 3_600,
            timezone: "UTC".to_string(),
        };
        r.pre_roll_secs = 15 * 60;
        r.post_roll_secs = 30 * 60;
        assert!(validate_rule(&r).is_ok());
    }

    #[test]
    fn validate_rule_rejects_new_episode_until_epg_horizon_is_wired() {
        // The scheduler has no EPG horizon yet, so a `NewEpisode` rule
        // would sit inert forever. Validation refuses it with a stable,
        // feature-specific code so the frontend can render an actionable
        // message and so removing the guard later is a single,
        // searchable edit.
        let mut r = private_rule(user());
        r.body = RuleBody::NewEpisode {
            series_id: Some("series-1".into()),
            title_pattern: Some("News".into()),
            exclude_repeat: false,
        };
        assert_eq!(
            validate_rule(&r),
            Err(RuleServiceError::Unsupported { feature: "new_episode_rule" })
        );
        assert_eq!(
            validate_rule(&r).err().map(|e| e.code()),
            Some("recording_rule_new_episode_unsupported")
        );
    }

    #[test]
    fn authorize_requires_recording_write() {
        let r = private_rule(user());
        assert_eq!(authorize_rule_action(false, false, &user(), &r), Err(RuleServiceError::Forbidden));
    }

    #[test]
    fn private_rule_owner_may_manage() {
        let r = private_rule(user());
        assert!(authorize_rule_action(true, false, &user(), &r).is_ok());
    }

    #[test]
    fn private_rule_other_user_may_not_manage_without_admin() {
        let r = private_rule(user());
        assert_eq!(authorize_rule_action(true, false, &other(), &r), Err(RuleServiceError::NotOwner));
    }

    #[test]
    fn administrator_may_manage_any_private_rule() {
        let r = private_rule(user());
        assert!(authorize_rule_action(true, true, &admin(), &r).is_ok());
    }

    #[test]
    fn shared_rule_requires_administrator() {
        let r = shared_rule();
        assert_eq!(
            authorize_rule_action(true, false, &user(), &r),
            Err(RuleServiceError::SharedManagementNotAdministrator)
        );
        assert!(authorize_rule_action(true, true, &admin(), &r).is_ok());
    }

    #[test]
    fn delete_future_parses_retain_and_cancel() {
        assert_eq!(DeleteFuture::parse("retain").unwrap(), DeleteFuture::Retain);
        assert_eq!(DeleteFuture::parse("cancel").unwrap(), DeleteFuture::Cancel);
        assert!(DeleteFuture::parse("bogus").is_err());
    }

    #[test]
    fn validate_delete_requires_retain_or_cancel() {
        assert!(matches!(validate_delete(None), Err(RuleServiceError::InvalidFuture)));
        assert!(matches!(validate_delete(Some("bogus")), Err(RuleServiceError::InvalidFuture)));
        assert!(validate_delete(Some("retain")).is_ok());
        assert!(validate_delete(Some("cancel")).is_ok());
    }

    #[test]
    fn cancel_targets_only_upcoming_inactive_tasks() {
        assert!(!cancel_targets_task(true, false));
        assert!(!cancel_targets_task(true, true));
        assert!(cancel_targets_task(false, true));
        assert!(!cancel_targets_task(false, false));
    }

    #[test]
    fn retain_targets_every_task() {
        assert!(retain_targets_task("u1"));
        assert!(retain_targets_task("u2"));
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(RuleServiceError::Forbidden.code(), "recording_rule_forbidden");
        assert_eq!(
            RuleServiceError::SharedManagementNotAdministrator.code(),
            "recording_shared_not_administrator"
        );
        assert_eq!(RuleServiceError::InvalidRule.code(), "recording_rule_invalid");
        assert_eq!(RuleServiceError::InvalidFuture.code(), "recording_rule_invalid_future");
        assert_eq!(RuleServiceError::UnknownRule.code(), "recording_rule_unknown");
        assert_eq!(RuleServiceError::NotOwner.code(), "recording_rule_not_owner");
        assert_eq!(RuleServiceError::PersistenceFailed.code(), "recording_persistence_failed");
        assert_eq!(
            RuleServiceError::PartialOperation {
                primary: "rule".into(),
                secondary: "tombstone".into()
            }
            .code(),
            "recording_rule_partial_operation"
        );
    }
}
