//! Recording rule + tombstone shared model.
//!
//! Recurring rules drive the rule scheduler. A rule has a stable
//! id, an owner + visibility, an enabled flag, a source/channel,
//! padding, a creation/update timestamp, and one of two rule
//! bodies:
//! - `NewEpisode` matches a programme on a stable EPG series id (or,
//!   as a fallback, a normalized title) that has not been seen before.
//! - `WeeklyTimeslot` matches every occurrence of a local
//!   wall-clock weekday + start time + duration in the configured
//!   IANA timezone.
//!
//! Tombstones are the durable suppression record. They outlive
//! their originating task so the scheduler does not rematerialize a
//! deleted or completed occurrence inside the horizon. Each
//! tombstone records the rule id, the occurrence key, the terminal
//! meaning (`Scheduled` / `Cancelled` / `Completed`), and the
//! relevant timestamps. Tombstones are pruned only after the
//! EPG horizon and the 14-day weekly fallback horizon have both
//! elapsed.

use crate::model::UserId;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Recording visibility. Same semantics as `RecordingVisibility` —
/// private tasks are visible to their owner, shared tasks to every
/// user with `recording.read`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleVisibility {
    #[default]
    Private,
    Shared,
}

/// Server-owned source identifiers. The stream URL is never accepted
/// from a client; the server resolves it from these identifiers at
/// execute time. Mirrors `RecordingSource` in the recording module.
///
/// `provider_id` and `is_group` are set by the server from the resolved source
/// and are ignored on input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSource {
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub is_group: bool,
}

impl RuleSource {
    pub fn new(target_id: impl Into<String>, virtual_id: impl Into<String>, input_name: impl Into<String>) -> Self {
        Self {
            target_id: target_id.into(),
            virtual_id: virtual_id.into(),
            input_name: input_name.into(),
            provider_id: None,
            is_group: false,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.target_id.trim().is_empty() || self.virtual_id.trim().is_empty() || self.input_name.trim().is_empty() {
            return Err("recording_rule_invalid_source");
        }
        Ok(())
    }
}

/// The matching body for a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleBody {
    /// Match every fresh episode on the channel.
    ///
    /// `series_id` is preferred when the EPG payload carries a
    /// stable series identifier. `title_pattern` is the normalized
    /// fallback used when the provider does not publish a stable
    /// id; the rule is explicitly limited to "may include reruns"
    /// in the UI when the fallback is in use.
    NewEpisode {
        #[serde(default)]
        series_id: Option<String>,
        #[serde(default)]
        title_pattern: Option<String>,
        /// When `true`, programmes with explicit `Repeat` airing
        /// status are excluded. Defaults to `true`.
        #[serde(default = "default_exclude_repeat")]
        exclude_repeat: bool,
    },
    /// Match a local wall-clock weekday + start time + duration
    /// inside the IANA timezone.
    ///
    /// `weekday` uses Monday = 1 .. Sunday = 7 (ISO-8601).
    /// `local_start_time` is `HH:MM` (24h). `duration_secs` is the
    /// programme length without padding.
    WeeklyTimeslot { weekday: u8, local_start_time: String, duration_secs: u64, timezone: String },
}

const fn default_exclude_repeat() -> bool { true }

impl RuleBody {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::NewEpisode { series_id, title_pattern, .. } => {
                let series_id_blank = series_id.as_deref().is_none_or(|s| s.trim().is_empty());
                let title_blank = title_pattern.as_deref().is_none_or(|s| s.trim().is_empty());
                if series_id_blank && title_blank {
                    return Err("recording_rule_invalid_match");
                }
                Ok(())
            }
            Self::WeeklyTimeslot { weekday, local_start_time, duration_secs, timezone } => {
                if !(1..=7).contains(weekday) {
                    return Err("recording_rule_invalid_weekday");
                }
                if duration_secs == &0 {
                    return Err("recording_rule_invalid_duration");
                }
                if timezone.parse::<Tz>().is_err() {
                    return Err("recording_rule_invalid_timezone");
                }
                // `local_start_time` must be `HH:MM`. The format
                // check is intentionally permissive — `13:00` and
                // `09:30` both pass; `9:00` and `13:00:00` fail.
                let mut parts = local_start_time.split(':');
                let h = parts.next().ok_or("recording_rule_invalid_local_time")?;
                let m = parts.next().ok_or("recording_rule_invalid_local_time")?;
                if parts.next().is_some() {
                    return Err("recording_rule_invalid_local_time");
                }
                if h.len() != 2 || m.len() != 2 {
                    return Err("recording_rule_invalid_local_time");
                }
                if h.parse::<u8>().map_err(|_| "recording_rule_invalid_local_time")? > 23 {
                    return Err("recording_rule_invalid_local_time");
                }
                if m.parse::<u8>().map_err(|_| "recording_rule_invalid_local_time")? > 59 {
                    return Err("recording_rule_invalid_local_time");
                }
                Ok(())
            }
        }
    }
}

/// A rule that drives the scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingRule {
    /// Stable rule id. Generated once, never changes.
    pub id: String,
    pub owner_id: UserId,
    pub visibility: RuleVisibility,
    pub enabled: bool,
    pub source: RuleSource,
    /// Optional stable channel id used as the matching fallback
    /// when a `WeeklyTimeslot` rule does not carry one explicitly.
    #[serde(default)]
    pub channel_id: Option<String>,
    pub body: RuleBody,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RecordingRule {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.source.validate()?;
        self.body.validate()?;
        Ok(())
    }
}

/// The terminal meaning of a tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneKind {
    /// The scheduler created a task for this occurrence.
    Scheduled,
    /// The user cancelled the task (terminal state `Cancelled`).
    Cancelled,
    /// The task reached `Completed` (the recording finished).
    Completed,
}

/// A bounded suppression record. The scheduler does not
/// re-materialize an occurrence inside the retention horizon once a
/// tombstone exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingTombstone {
    pub rule_id: String,
    /// Versioned canonical occurrence key. See `recording_occurrence`
    /// for the layout.
    pub occurrence_key: String,
    pub kind: TombstoneKind,
    /// UTC Unix seconds when the tombstone was created.
    pub created_at: i64,
    /// UTC Unix seconds the tombstone expires at. Computed by the
    /// repository from the rule's matching window.
    pub expires_at: i64,
}

/// The bounded set of tombstones. The repository loads the file,
/// prunes tombstones whose `expires_at` is in the past, and persists
/// the result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombstoneSet {
    pub tombstones: Vec<RecordingTombstone>,
}

/// The on-disk shape. The repository reads and writes the file in
/// one atomic operation; the queue-mutation boundary does the
/// in-memory swap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingRulesFile {
    /// Schema version. Bumped when the on-disk shape changes.
    #[serde(default)]
    pub version: u32,
    pub rules: Vec<RecordingRule>,
    pub tombstones: TombstoneSet,
}

/// User-facing source identifiers of a recording rule for playlist user clients.
/// Omits provider internals (`provider_id`, `input_name`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRuleSourceDto {
    pub target_id: String,
    pub virtual_id: String,
    pub is_group: bool,
}

impl From<&RuleSource> for UserRuleSourceDto {
    fn from(source: &RuleSource) -> Self {
        Self { target_id: source.target_id.clone(), virtual_id: source.virtual_id.clone(), is_group: source.is_group }
    }
}

/// Redacted recording rule response for playlist user clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRecordingRuleDto {
    pub revision: u64,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<UserId>,
    pub own: bool,
    pub visibility: RuleVisibility,
    pub enabled: bool,
    pub source: UserRuleSourceDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    pub body: RuleBody,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl UserRecordingRuleDto {
    pub fn from_rule(rule: &RecordingRule, revision: u64, is_own: bool) -> Self {
        Self {
            revision,
            id: rule.id.clone(),
            owner_id: if is_own { Some(rule.owner_id.clone()) } else { None },
            own: is_own,
            visibility: rule.visibility,
            enabled: rule.enabled,
            source: UserRuleSourceDto::from(&rule.source),
            channel_id: rule.channel_id.clone(),
            body: rule.body.clone(),
            pre_roll_secs: rule.pre_roll_secs,
            post_roll_secs: rule.post_roll_secs,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserId;

    fn user() -> UserId { UserId::from("web:alice") }

    fn source() -> RuleSource { RuleSource::new("tgt", "virt", "input") }

    #[test]
    fn rule_source_rejects_empty_fields() {
        assert!(RuleSource::new("", "v", "i").validate().is_err());
        assert!(RuleSource::new("t", "", "i").validate().is_err());
        assert!(RuleSource::new("t", "v", "").validate().is_err());
    }

    #[test]
    fn new_episode_rule_requires_series_or_title() {
        let body = RuleBody::NewEpisode { series_id: None, title_pattern: None, exclude_repeat: true };
        assert!(body.validate().is_err());
        let body =
            RuleBody::NewEpisode { series_id: Some("series-1".into()), title_pattern: None, exclude_repeat: true };
        assert!(body.validate().is_ok());
    }

    #[test]
    fn weekly_rule_validates_timezone_and_time() {
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "Europe/Berlin".into(),
        };
        assert!(body.validate().is_ok());
        // Unknown timezone.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "Not/AZone".into(),
        };
        assert!(body.validate().is_err());
    }

    #[test]
    fn weekly_rule_validates_weekday() {
        let body = RuleBody::WeeklyTimeslot {
            weekday: 0,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
        let body = RuleBody::WeeklyTimeslot {
            weekday: 8,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
    }

    #[test]
    fn weekly_rule_validates_duration() {
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".into(),
            duration_secs: 0,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
    }

    #[test]
    fn weekly_rule_validates_local_time_format() {
        // `9:00` is not `HH:MM` — reject.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "9:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
        // Three-component form — reject.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "09:00:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
        // Out-of-range minutes — reject.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:99".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        assert!(body.validate().is_err());
    }

    #[test]
    fn rule_validate_rejects_invalid_source_and_body() {
        let mut rule = RecordingRule {
            id: "r1".into(),
            owner_id: user(),
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
        };
        assert!(rule.validate().is_ok());
        rule.source = RuleSource::new("", "virt", "input");
        assert!(rule.validate().is_err());
    }

    #[test]
    fn tombstone_set_round_trip() {
        let set = TombstoneSet {
            tombstones: vec![RecordingTombstone {
                rule_id: "r1".into(),
                occurrence_key: "ok-1".into(),
                kind: TombstoneKind::Scheduled,
                created_at: 100,
                expires_at: 1_000_000,
            }],
        };
        let json = serde_json::to_string(&set).unwrap();
        let back: TombstoneSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set);
    }

    #[test]
    fn rules_file_round_trip() {
        let rule = RecordingRule {
            id: "r1".into(),
            owner_id: user(),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: source(),
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
        };
        let file = RecordingRulesFile {
            version: 1,
            rules: vec![rule.clone()],
            tombstones: TombstoneSet { tombstones: vec![] },
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: RecordingRulesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn user_recording_rule_dto_redacts_internals_and_foreign_owner() {
        let mut source = RuleSource::new("target1", "100", "providerA");
        source.provider_id = Some("prov-123".to_string());
        source.is_group = false;

        let rule = RecordingRule {
            id: "rule-1".into(),
            owner_id: UserId::api("bob"),
            visibility: RuleVisibility::Shared,
            enabled: true,
            source,
            channel_id: Some("ch-1".to_string()),
            body: RuleBody::WeeklyTimeslot {
                weekday: 2,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 10,
            post_roll_secs: 20,
            created_at: 1000,
            updated_at: 2000,
        };

        // When viewed by Bob (own rule): own is true, owner_id is present
        let own_dto = UserRecordingRuleDto::from_rule(&rule, 42, true);
        let own_json: serde_json::Value = serde_json::to_value(&own_dto).unwrap();
        assert_eq!(own_json["own"], true);
        assert_eq!(own_json["owner_id"], "api:bob");
        assert_eq!(own_json["source"]["target_id"], "target1");
        assert_eq!(own_json["source"]["virtual_id"], "100");
        assert_eq!(own_json["source"]["is_group"], false);
        assert!(own_json["source"].get("provider_id").is_none());
        assert!(own_json["source"].get("input_name").is_none());

        // When viewed by Alice (foreign rule): own is false, owner_id is omitted
        let foreign_dto = UserRecordingRuleDto::from_rule(&rule, 42, false);
        let foreign_json: serde_json::Value = serde_json::to_value(&foreign_dto).unwrap();
        assert_eq!(foreign_json["own"], false);
        assert!(foreign_json.get("owner_id").is_none(), "owner_id must be omitted for foreign rules");
        assert_eq!(foreign_json["source"]["target_id"], "target1");
        assert_eq!(foreign_json["source"]["virtual_id"], "100");
        assert!(foreign_json["source"].get("provider_id").is_none());
        assert!(foreign_json["source"].get("input_name").is_none());
    }
}
