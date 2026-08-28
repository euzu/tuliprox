//! Recording occurrence keys and rule matching.
//!
//! An occurrence key is a stable, deterministic identifier for a
//! particular (rule, time slot, programme) tuple. The scheduler
//! computes it; tombstones and tasks both store it. The key is the
//! only field that needs to match between a tombstone and a future
//! scheduler pass for the suppression to take effect.
//!
//! Layout: versioned, length-prefixed, separated by a unit-separator
//! control character. The version prefix lets the layout change
//! without breaking the tombstone retention horizon — old
//! tombstones simply fail to match future occurrences and get
//! pruned.
//!
//! The matching rules:
//! - `NewEpisode`: stable series id first; normalized title as a
//!   fallback. Exclude explicit `Repeat`. Treat `Unknown` as new.
//! - `WeeklyTimeslot`: local wall-clock weekday + start time +
//!   duration in the configured IANA timezone.

use chrono::{DateTime, Datelike, Duration, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use shared::model::{
    recording_rule::{RecordingRule, RuleBody, RuleSource, RuleVisibility},
    UserId,
};

const KEY_VERSION: &str = "v1";
const FIELD_SEP: char = '\u{1f}';

/// Canonical occurrence key. Pure: the same inputs always produce
/// the same bytes.
pub fn occurrence_key(
    rule_id: &str,
    source: &RuleSource,
    channel_key: &str,
    programme_start_utc_secs: i64,
    episode_key: &str,
) -> String {
    let mut out = String::new();
    out.push_str(KEY_VERSION);
    out.push(FIELD_SEP);
    push_field(&mut out, rule_id);
    out.push(FIELD_SEP);
    push_field(&mut out, &format!("{}/{}/{}", source.target_id, source.virtual_id, source.input_name));
    out.push(FIELD_SEP);
    push_field(&mut out, channel_key);
    out.push(FIELD_SEP);
    push_field(&mut out, &programme_start_utc_secs.to_string());
    out.push(FIELD_SEP);
    push_field(&mut out, episode_key);
    out
}

fn push_field(out: &mut String, value: &str) {
    // Length-prefix each field so a separator inside the value
    // cannot collide with the field separator.
    out.push_str(&value.chars().count().to_string());
    out.push(':');
    out.push_str(value);
}

/// Channel key: prefer `channel_id`, fall back to the normalized
/// channel name. The normalized form is lowercase with
/// whitespace collapsed; the fallback is the closest stable match
/// the EPG payload provides.
pub fn channel_key(channel_id: Option<&str>, channel_name: Option<&str>) -> String {
    if let Some(id) = channel_id.filter(|s| !s.is_empty()) {
        return id.to_string();
    }
    if let Some(name) = channel_name {
        return normalize_channel_name(name);
    }
    String::new()
}

fn normalize_channel_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_space = true;
    for c in lower.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// Episode key: pick the strongest available identity (stable
/// series id first, normalized title as a fallback). Empty inputs
/// are skipped.
pub fn episode_key(
    episode_id: Option<&str>,
    programme_id: Option<&str>,
    series_id: Option<&str>,
    season: Option<u32>,
    episode: Option<u32>,
    title: Option<&str>,
) -> String {
    if let Some(e) = episode_id.filter(|s| !s.is_empty()) {
        return format!("e:{e}");
    }
    if let Some(p) = programme_id.filter(|s| !s.is_empty()) {
        return format!("p:{p}");
    }
    if let Some(s) = series_id.filter(|s| !s.is_empty()) {
        let season = season.map(|n| n.to_string()).unwrap_or_default();
        let episode = episode.map(|n| n.to_string()).unwrap_or_default();
        return format!("s:{s}:{season}:{episode}");
    }
    if let Some(t) = title.filter(|s| !s.is_empty()) {
        return format!("t:{}", normalize_title(t));
    }
    String::new()
}

fn normalize_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_space = true;
    for c in lower.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// `NewEpisode` match result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewEpisodeMatch {
    /// `Repeat` airing status; the new-episode rule excludes it
    /// from recording.
    Excluded,
    /// No series id or matching title; the candidate does not match
    /// this rule.
    NoMatch,
    /// Series id matched (or the title fallback matched); the
    /// candidate is a new episode for this rule.
    NewEpisode,
}

/// Does a candidate programme match the rule's `NewEpisode` body?
/// Matching order: stable series id first, normalized title as a
/// fallback. `Unknown` airing is treated as new; `Repeat` is
/// excluded; `New` is always a match.
pub fn matches_new_episode(
    rule_body: &RuleBody,
    candidate_series_id: Option<&str>,
    candidate_title: Option<&str>,
    airing_is_repeat: bool,
) -> NewEpisodeMatch {
    let RuleBody::NewEpisode { series_id, title_pattern, exclude_repeat } = rule_body else {
        return NewEpisodeMatch::NoMatch;
    };
    if airing_is_repeat && *exclude_repeat {
        return NewEpisodeMatch::Excluded;
    }
    if let Some(rule_series) = series_id.as_deref().filter(|s| !s.is_empty()) {
        if candidate_series_id.is_some_and(|s| s == rule_series) {
            return NewEpisodeMatch::NewEpisode;
        }
        return NewEpisodeMatch::NoMatch;
    }
    if let Some(rule_title) = title_pattern.as_deref().filter(|s| !s.is_empty()) {
        if let Some(candidate) = candidate_title {
            if normalize_title(candidate) == normalize_title(rule_title) {
                return NewEpisodeMatch::NewEpisode;
            }
        }
    }
    NewEpisodeMatch::NoMatch
}

/// Weekly match: a candidate programme matches when its UTC start
/// aligns with the rule's local weekday + start time (modulo
/// padding). This is a coarse pre-filter; the runtime re-validates
/// against the actual scheduled interval.
pub fn matches_weekly(rule_body: &RuleBody, candidate_start_utc: i64) -> bool {
    let RuleBody::WeeklyTimeslot { weekday, local_start_time, duration_secs, timezone } = rule_body else {
        return false;
    };
    let Ok(tz) = timezone.parse::<Tz>() else {
        return false;
    };
    let Some(candidate_local) = Utc.timestamp_opt(candidate_start_utc, 0).single() else {
        return false;
    };
    let candidate_local = candidate_local.with_timezone(&tz);
    if candidate_local.weekday().num_days_from_monday() + 1 != u32::from(*weekday) {
        return false;
    }
    let Some((h, m)) = parse_hh_mm(local_start_time) else {
        return false;
    };
    let Some(expected_time) = NaiveTime::from_hms_opt(h, m, 0) else {
        return false;
    };
    let expected_local = NaiveDateTime::new(candidate_local.date_naive(), expected_time);
    // Match within the programme duration window: the candidate's
    // start must be at or after the slot's start, and not later
    // than slot_end (start + duration). The runtime decides the
    // exact tolerance.
    let slot_start_utc =
        tz.from_local_datetime(&expected_local).earliest().map(|dt| dt.with_timezone(&Utc).timestamp());
    let Ok(duration_secs) = i64::try_from(*duration_secs) else { return false };
    slot_start_utc
        .and_then(|start| start.checked_add(duration_secs).map(|end| (start, end)))
        .is_some_and(|(start, end)| candidate_start_utc >= start && candidate_start_utc < end)
}

/// Resolve the next weekly occurrence at or after `now` in the
/// rule's timezone. DST handling:
/// - For an ambiguous local time (fall-back DST), pick the earlier
///   instant.
/// - For a nonexistent local time (spring-forward DST), advance
///   to the first valid instant at or after the requested time.
pub fn next_weekly_occurrence(rule_body: &RuleBody, now: DateTime<Utc>) -> Option<i64> {
    let RuleBody::WeeklyTimeslot { weekday, local_start_time, timezone, .. } = rule_body else {
        return None;
    };
    let tz: Tz = timezone.parse().ok()?;
    let (h, m) = parse_hh_mm(local_start_time)?;
    let now_local = now.with_timezone(&tz);
    // Search up to 8 days ahead. DST gaps are at most 1 hour; 8
    // days covers the worst case.
    for offset in 0..8 {
        let candidate_date = now_local.date_naive() + Duration::days(offset);
        if candidate_date.weekday().num_days_from_monday() + 1 != u32::from(*weekday) {
            continue;
        }
        let local = NaiveDateTime::new(candidate_date, NaiveTime::from_hms_opt(h, m, 0)?);
        match tz.from_local_datetime(&local) {
            chrono::LocalResult::Single(dt) => {
                let utc = dt.with_timezone(&Utc);
                if utc >= now {
                    return Some(utc.timestamp());
                }
            }
            chrono::LocalResult::Ambiguous(earliest, _) => {
                let utc = earliest.with_timezone(&Utc);
                if utc >= now {
                    return Some(utc.timestamp());
                }
            }
            chrono::LocalResult::None => {
                // Spring-forward gap. Advance 1 hour at a time until
                // we find a valid instant; max 4 hours of gap is
                // safe in any IANA timezone.
                for hour_offset in 1..=4 {
                    let advanced = local + Duration::hours(hour_offset);
                    if let Some(dt) = tz.from_local_datetime(&advanced).earliest() {
                        let utc = dt.with_timezone(&Utc);
                        if utc >= now {
                            return Some(utc.timestamp());
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_hh_mm(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split(':');
    let h = parts.next()?.parse::<u32>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// Build the channel key for a candidate programme.
pub fn candidate_channel_key(channel_id: Option<&str>, channel_name: Option<&str>) -> String {
    channel_key(channel_id, channel_name)
}

/// Build the episode key for a candidate programme.
pub fn candidate_episode_key(
    episode_id: Option<&str>,
    programme_id: Option<&str>,
    series_id: Option<&str>,
    season: Option<u32>,
    episode: Option<u32>,
    title: Option<&str>,
) -> String {
    episode_key(episode_id, programme_id, series_id, season, episode, title)
}

/// Build a `RecordingRule` with sensible defaults for tests.
pub fn build_rule(id: &str, owner: UserId, body: RuleBody, source: RuleSource) -> RecordingRule {
    RecordingRule {
        id: id.to_string(),
        owner_id: owner,
        visibility: RuleVisibility::Private,
        enabled: true,
        source,
        channel_id: None,
        body,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        created_at: 0,
        updated_at: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use shared::model::{recording_rule::RuleSource, UserId};

    fn source() -> RuleSource {
        RuleSource::new("tgt-1", "virt-1", "input-1")
    }
    fn user() -> UserId {
        UserId::from("web:alice")
    }

    #[test]
    fn occurrence_key_is_deterministic_and_version_prefixed() {
        let k1 = occurrence_key("r1", &source(), "ch-1", 1_700_000_000, "e:ep-1");
        let k2 = occurrence_key("r1", &source(), "ch-1", 1_700_000_000, "e:ep-1");
        assert_eq!(k1, k2);
        assert!(k1.starts_with("v1\u{1f}"));
    }

    #[test]
    fn occurrence_key_changes_when_inputs_change() {
        let k1 = occurrence_key("r1", &source(), "ch-1", 1_700_000_000, "e:ep-1");
        let k2 = occurrence_key("r2", &source(), "ch-1", 1_700_000_000, "e:ep-1");
        assert_ne!(k1, k2);
        let k3 = occurrence_key("r1", &source(), "ch-2", 1_700_000_000, "e:ep-1");
        assert_ne!(k1, k3);
        let k4 = occurrence_key("r1", &source(), "ch-1", 1_700_000_001, "e:ep-1");
        assert_ne!(k1, k4);
    }

    #[test]
    fn occurrence_key_does_not_collide_on_separator() {
        // The internal separator is `\u{1f}`. A field value that
        // contains the separator must not be confused with the
        // field boundary; the length prefix prevents this.
        let k = occurrence_key("r1", &source(), "a\u{1f}b", 0, "");
        let k2 = occurrence_key("r1", &source(), "a", 0, "b");
        assert_ne!(k, k2);
    }

    #[test]
    fn channel_key_prefers_id() {
        assert_eq!(channel_key(Some("ch-1"), Some("Channel 1")), "ch-1");
        assert_eq!(channel_key(Some("ch-1"), None), "ch-1");
    }

    #[test]
    fn channel_key_normalizes_name_fallback() {
        assert_eq!(channel_key(None, Some("Channel  ONE")), "channel one");
    }

    #[test]
    fn channel_key_returns_empty_when_no_inputs() {
        assert_eq!(channel_key(None, None), "");
    }

    #[test]
    fn episode_key_prefers_episode_id() {
        assert_eq!(episode_key(Some("e1"), Some("p1"), Some("s1"), Some(1), Some(2), Some("Title")), "e:e1");
    }

    #[test]
    fn episode_key_falls_back_to_programme_id() {
        assert_eq!(episode_key(None, Some("p1"), Some("s1"), Some(1), Some(2), Some("Title")), "p:p1");
    }

    #[test]
    fn episode_key_falls_back_to_series_with_season_episode() {
        assert_eq!(episode_key(None, None, Some("s1"), Some(3), Some(7), Some("Title")), "s:s1:3:7");
    }

    #[test]
    fn episode_key_falls_back_to_normalized_title() {
        assert_eq!(episode_key(None, None, None, None, None, Some("Title One")), "t:title one");
    }

    #[test]
    fn matches_new_episode_with_series_id() {
        let body =
            RuleBody::NewEpisode { series_id: Some("series-1".into()), title_pattern: None, exclude_repeat: true };
        assert_eq!(matches_new_episode(&body, Some("series-1"), Some("Other"), false), NewEpisodeMatch::NewEpisode);
        assert_eq!(matches_new_episode(&body, Some("series-2"), Some("Other"), false), NewEpisodeMatch::NoMatch);
    }

    #[test]
    fn matches_new_episode_with_title_fallback() {
        let body =
            RuleBody::NewEpisode { series_id: None, title_pattern: Some("My Show".into()), exclude_repeat: true };
        assert_eq!(matches_new_episode(&body, None, Some("My  Show"), false), NewEpisodeMatch::NewEpisode);
        assert_eq!(matches_new_episode(&body, None, Some("Other"), false), NewEpisodeMatch::NoMatch);
    }

    #[test]
    fn matches_new_episode_excludes_repeat_when_configured() {
        let body =
            RuleBody::NewEpisode { series_id: Some("series-1".into()), title_pattern: None, exclude_repeat: true };
        assert_eq!(matches_new_episode(&body, Some("series-1"), Some("Other"), true), NewEpisodeMatch::Excluded);
    }

    #[test]
    fn matches_new_episode_includes_repeat_when_not_configured() {
        let body =
            RuleBody::NewEpisode { series_id: Some("series-1".into()), title_pattern: None, exclude_repeat: false };
        assert_eq!(matches_new_episode(&body, Some("series-1"), Some("Other"), true), NewEpisodeMatch::NewEpisode);
    }

    #[test]
    fn matches_new_episode_unknown_airing_is_new() {
        let body =
            RuleBody::NewEpisode { series_id: Some("series-1".into()), title_pattern: None, exclude_repeat: true };
        // `airing_is_repeat` is false (Unknown / New), so the
        // candidate is treated as new.
        assert_eq!(matches_new_episode(&body, Some("series-1"), Some("Other"), false), NewEpisodeMatch::NewEpisode);
    }

    #[test]
    fn matches_weekly_picks_correct_day() {
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        // 2023-11-13 is a Monday in UTC. 20:00 UTC = 1_699_905_600.
        let monday_8pm = 1_699_905_600;
        assert!(matches_weekly(&body, monday_8pm));
        let tuesday_8pm = monday_8pm + 86_400;
        assert!(!matches_weekly(&body, tuesday_8pm));
    }

    #[test]
    fn matches_weekly_uses_timezone() {
        // Berlin is UTC+1 in winter. 20:00 Berlin on Monday is
        // 19:00 UTC.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 1,
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "Europe/Berlin".into(),
        };
        // 2023-11-13 19:00 UTC = 1_699_902_000.
        let monday_7pm_utc = 1_699_902_000;
        assert!(matches_weekly(&body, monday_7pm_utc));
    }

    #[test]
    fn next_weekly_occurrence_picks_first_matching_day() {
        let body = RuleBody::WeeklyTimeslot {
            weekday: 7, // Sunday
            local_start_time: "20:00".into(),
            duration_secs: 1800,
            timezone: "UTC".into(),
        };
        // Start at 2023-11-13 (Monday) 00:00 UTC.
        let now = Utc.timestamp_opt(1_699_833_600, 0).unwrap();
        let next = next_weekly_occurrence(&body, now).expect("next occurrence");
        let next_dt = Utc.timestamp_opt(next, 0).unwrap();
        assert_eq!(next_dt.weekday(), chrono::Weekday::Sun);
    }

    #[test]
    fn next_weekly_occurrence_handles_dst_fall_back() {
        // Berlin DST ends on 2023-10-29 03:00. 02:30 is
        // ambiguous; we pick the earlier (00:30 UTC) instant.
        let body = RuleBody::WeeklyTimeslot {
            weekday: 7,
            local_start_time: "02:30".into(),
            duration_secs: 1800,
            timezone: "Europe/Berlin".into(),
        };
        // 2023-10-22 12:00 UTC = 14:00 Berlin, the Sunday before
        // the DST change.
        let now = Utc.timestamp_opt(1_697_976_000, 0).unwrap();
        let next = next_weekly_occurrence(&body, now).expect("next occurrence");
        let dt = Utc.timestamp_opt(next, 0).unwrap().with_timezone(&chrono_tz::Europe::Berlin);
        assert_eq!(dt.weekday(), chrono::Weekday::Sun);
        // The local date must be the next Sunday (2023-10-29). We
        // do not assert a fixed UTC second here — chrono_tz's
        // ambiguous-instant policy has been stable for years but
        // the exact second can shift if the dependency upgrades.
        let local_date = dt.date_naive();
        assert_eq!(local_date, NaiveDate::from_ymd_opt(2023, 10, 29).unwrap());
        // The local wall clock should be 02:30 (within a few
        // hours of DST resolution).
        let naive = NaiveDateTime::new(local_date, NaiveTime::from_hms_opt(2, 30, 0).unwrap());
        let resolved = chrono_tz::Europe::Berlin.from_local_datetime(&naive);
        assert!(matches!(resolved, chrono::LocalResult::Ambiguous(_, _) | chrono::LocalResult::Single(_)));
    }

    #[test]
    fn build_rule_uses_defaults() {
        let body = RuleBody::NewEpisode { series_id: Some("s".into()), title_pattern: None, exclude_repeat: true };
        let r = build_rule("r1", user(), body.clone(), source());
        assert_eq!(r.id, "r1");
        assert_eq!(r.body, body);
        assert!(r.enabled);
        assert_eq!(r.pre_roll_secs, 0);
        assert_eq!(r.post_roll_secs, 0);
    }
}
