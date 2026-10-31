use shared::model::{TargetAnalyticsDto, TargetAnalyticsEntryDto, TargetType};
use std::rc::Rc;

pub trait Translator {
    fn t(&self, key: &str) -> String;
}

impl Translator for crate::i18n::YewI18n {
    fn t(&self, key: &str) -> String { self.t(key) }
}

impl<F: Fn(&str) -> String> Translator for F {
    fn t(&self, key: &str) -> String { self(key) }
}

/// Formats a translated template by replacing `{placeholder}` keys with values.
pub fn t_fmt(translate: &impl Translator, key: &str, args: &[(&str, &str)]) -> String {
    let mut text = translate.t(key);
    for (k, v) in args {
        let needle = format!("{{{k}}}");
        text = text.replace(&needle, v);
    }
    text
}

/// Like [`t_fmt`], but splits the result around one placeholder so the caller can emphasize its value.
/// Returns `(before, value, after)`; when the template lacks the placeholder, `before` holds the whole text.
pub fn t_fmt_split(
    translate: &impl Translator,
    key: &str,
    highlight: (&str, &str),
    args: &[(&str, &str)],
) -> (String, String, String) {
    let text = t_fmt(translate, key, args);
    let needle = format!("{{{}}}", highlight.0);
    match text.split_once(&needle) {
        Some((before, after)) => (before.to_string(), highlight.1.to_string(), after.to_string()),
        None => (text, String::new(), String::new()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiTargetAnalyticsEntry {
    pub target_id: u16,
    pub name: String,
    pub outputs: Rc<Vec<TargetType>>,
    pub next_scheduled: Option<i64>,
    pub analytics: Option<Rc<TargetAnalyticsDto>>,
}

impl From<TargetAnalyticsEntryDto> for UiTargetAnalyticsEntry {
    fn from(dto: TargetAnalyticsEntryDto) -> Self {
        let analytics = dto.analytics.map(Rc::new);
        Self {
            target_id: dto.target_id,
            name: dto.name,
            outputs: Rc::new(dto.outputs),
            next_scheduled: dto.next_scheduled,
            analytics,
        }
    }
}

/// Derives the effective selected target from user choice and the targets list:
/// 1. User choice if it exists in the list
/// 2. Else first target with analytics
/// 3. Else first target in the list
#[must_use]
pub fn derive_effective_target(
    user_choice: Option<u16>,
    targets: &[Rc<UiTargetAnalyticsEntry>],
) -> Option<&Rc<UiTargetAnalyticsEntry>> {
    if let Some(chosen_id) = user_choice {
        if let Some(target) = targets.iter().find(|t| t.target_id == chosen_id) {
            return Some(target);
        }
    }
    targets.iter().find(|t| t.analytics.is_some()).or_else(|| targets.first())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Ok,
    EpgStale,
    Error,
    NoData,
}

impl TargetStatus {
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Ok => "LABEL.PLAYLIST_STATS_STATUS_OK",
            Self::EpgStale => "LABEL.PLAYLIST_STATS_STATUS_EPG_STALE",
            Self::Error => "LABEL.PLAYLIST_STATS_STATUS_ERROR",
            Self::NoData => "LABEL.PLAYLIST_STATS_STATUS_NO_DATA",
        }
    }

    pub const fn badge_class(self) -> &'static str {
        match self {
            Self::Ok => "tp__playlist-stats__badge tp__playlist-stats__badge--ok",
            Self::EpgStale => "tp__playlist-stats__badge tp__playlist-stats__badge--stale",
            Self::Error => "tp__playlist-stats__badge tp__playlist-stats__badge--error",
            Self::NoData => "tp__playlist-stats__badge tp__playlist-stats__badge--nodata",
        }
    }
}

/// Computes percentage with one decimal place. Returns `None` when `total == 0`.
#[must_use]
pub fn percent(count: usize, total: usize) -> Option<f64> {
    if total == 0 {
        None
    } else {
        let val = (count as f64 / total as f64) * 100.0;
        Some((val * 10.0).round() / 10.0)
    }
}

/// Classifies the status of a target.
///
/// - `NoData`: Target has no analytics yet (`analytics == None`).
/// - `Error`: Last update run failed (`!analytics.success`).
/// - `EpgStale`: Target has EPG and at least one source failed during the last run
///   or the guide window ends within 24 hours from `now_secs`.
/// - `Ok`: Succeeded without stale EPG, or target has no EPG configured at all.
#[must_use]
pub fn classify_target_status(analytics: Option<&TargetAnalyticsDto>, now_secs: i64) -> TargetStatus {
    let Some(data) = analytics else {
        return TargetStatus::NoData;
    };

    if !data.success {
        return TargetStatus::Error;
    }

    if let Some(ref epg) = data.epg {
        let has_failed_source = epg.sources.iter().any(|s| s.failed_at.is_some());
        let guide_expiring_soon = epg.guide_until.is_some_and(|until| until < now_secs + 24 * 3600);
        if has_failed_source || guide_expiring_soon {
            return TargetStatus::EpgStale;
        }
    }

    TargetStatus::Ok
}

/// Formats a number with commas as thousands separators (e.g. 18432 -> "18,432").
#[must_use]
pub fn format_number(val: usize) -> String {
    let s = val.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(*b as char);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelativeTime {
    JustNow,
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

/// Classifies elapsed time between `timestamp` and `now` into relative buckets.
#[must_use]
pub fn relative_time(timestamp: u64, now: u64) -> RelativeTime {
    let elapsed = now.saturating_sub(timestamp);
    if elapsed < 60 {
        RelativeTime::JustNow
    } else if elapsed < 3_600 {
        RelativeTime::Minutes(elapsed / 60)
    } else if elapsed < 86_400 {
        RelativeTime::Hours(elapsed / 3_600)
    } else {
        RelativeTime::Days(elapsed / 86_400)
    }
}

/// Formats relative time using translation keys.
pub fn format_relative_time(rel: RelativeTime, translate: &impl Translator) -> String {
    match rel {
        RelativeTime::JustNow => translate.t("LABEL.PLAYLIST_STATS_TIME_JUST_NOW"),
        RelativeTime::Minutes(count) => {
            let count_str = count.to_string();
            t_fmt(translate, "LABEL.PLAYLIST_STATS_TIME_MINS_AGO", &[("count", &count_str)])
        }
        RelativeTime::Hours(count) => {
            let count_str = count.to_string();
            t_fmt(translate, "LABEL.PLAYLIST_STATS_TIME_HOURS_AGO", &[("count", &count_str)])
        }
        RelativeTime::Days(count) => {
            let count_str = count.to_string();
            t_fmt(translate, "LABEL.PLAYLIST_STATS_TIME_DAYS_AGO", &[("count", &count_str)])
        }
    }
}

/// Formats scheduled unix timestamp into local HH:MM if today, or YYYY-MM-DD HH:MM if on another day.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn format_scheduled_time(ts: i64, now_ts: i64) -> String {
    let sched_date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
    let now_date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(now_ts as f64 * 1000.0));
    let is_today = sched_date.get_full_year() == now_date.get_full_year()
        && sched_date.get_month() == now_date.get_month()
        && sched_date.get_date() == now_date.get_date();
    if is_today {
        format!("{:02}:{:02}", sched_date.get_hours(), sched_date.get_minutes())
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            sched_date.get_full_year(),
            sched_date.get_month() + 1,
            sched_date.get_date(),
            sched_date.get_hours(),
            sched_date.get_minutes()
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn format_scheduled_time(ts: i64, now_ts: i64) -> String {
    use chrono::{Local, TimeZone};
    let sched_dt = Local.timestamp_opt(ts, 0).single();
    let now_dt = Local.timestamp_opt(now_ts, 0).single();
    if let (Some(s), Some(n)) = (sched_dt, now_dt) {
        if s.date_naive() == n.date_naive() {
            s.format("%H:%M").to_string()
        } else {
            s.format("%Y-%m-%d %H:%M").to_string()
        }
    } else {
        ts.to_string()
    }
}

/// Formats a guide end timestamp as a local date string (e.g. "YYYY-MM-DD" or "Nov 3").
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn format_guide_until_date(ts: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
    format!("{:04}-{:02}-{:02}", date.get_full_year(), date.get_month() + 1, date.get_date())
}

#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn format_guide_until_date(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(ts, 0).single().map_or_else(|| ts.to_string(), |dt| dt.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        ClusterAnalytics, EpgAnalytics, EpgSourceAnalytics, EpgSourceTypeDto, TARGET_ANALYTICS_VERSION,
    };

    fn make_test_analytics(success: bool, epg: Option<EpgAnalytics>) -> TargetAnalyticsDto {
        TargetAnalyticsDto {
            version: TARGET_ANALYTICS_VERSION,
            success,
            updated_at: 1_700_000_000,
            took_secs: 15,
            inputs: vec!["p1".into()],
            live: ClusterAnalytics { count: 100, groups: 10, input_count: 120 },
            vod: ClusterAnalytics { count: 50, groups: 5, input_count: 60 },
            series: ClusterAnalytics { count: 20, groups: 2, input_count: 25 },
            episodes: Some(400),
            episodes_complete: true,
            catchup_channels: 10,
            catchup_max_days: Some(7),
            top_live_groups: Vec::new(),
            epg,
        }
    }

    #[test]
    fn stats_translation_keys_exist_in_en_json() {
        let en: serde_json::Value =
            serde_json::from_str(include_str!("../../public/assets/i18n/en.json")).expect("en.json");
        let sources = [
            include_str!("playlist_stats_model.rs"),
            include_str!("../app/components/playlist/stats/bar_row.rs"),
            include_str!("../app/components/playlist/stats/epg_coverage_panel.rs"),
            include_str!("../app/components/playlist/stats/epg_coverage_ring.rs"),
            include_str!("../app/components/playlist/stats/epg_source_list.rs"),
            include_str!("../app/components/playlist/stats/input_output_panel.rs"),
            include_str!("../app/components/playlist/stats/kpi_card.rs"),
            include_str!("../app/components/playlist/stats/kpi_row.rs"),
            include_str!("../app/components/playlist/stats/missing_epg_chips.rs"),
            include_str!("../app/components/playlist/stats/playlist_stats_view.rs"),
            include_str!("../app/components/playlist/stats/stats_meta_line.rs"),
            include_str!("../app/components/playlist/stats/targets_table.rs"),
        ];
        let mut missing = Vec::new();
        for source in sources {
            // Only the production part; the test module itself contains key-like literals.
            let source = source.split("#[cfg(test)]").next().unwrap_or(source);
            for part in source.split('"').skip(1).step_by(2) {
                if (part.starts_with("LABEL.") || part.starts_with("MESSAGES."))
                    && part.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '.')
                    && part.split('.').try_fold(&en, |node, seg| node.get(seg)).is_none()
                {
                    missing.push(part.to_string());
                }
            }
        }
        assert!(missing.is_empty(), "missing i18n keys: {missing:?}");
    }

    #[test]
    fn test_percent() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(10, 0), None);
        assert_eq!(percent(0, 100), Some(0.0));
        assert_eq!(percent(50, 100), Some(50.0));
        assert_eq!(percent(18_432, 42_118), Some(43.8));
        assert_eq!(percent(64_210, 128_440), Some(50.0));
        assert_eq!(percent(9_874, 21_077), Some(46.8));
        assert_eq!(percent(16_036, 18_432), Some(87.0));
        assert_eq!(percent(100, 100), Some(100.0));
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(9), "9");
        assert_eq!(format_number(99), "99");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(18_432), "18,432");
        assert_eq!(format_number(64_210), "64,210");
        assert_eq!(format_number(286_441), "286,441");
        assert_eq!(format_number(1_000_000), "1,000,000");
    }

    #[test]
    fn test_relative_time() {
        let now = 1_000_000;
        assert_eq!(relative_time(now, now), RelativeTime::JustNow);
        assert_eq!(relative_time(now - 59, now), RelativeTime::JustNow);
        assert_eq!(relative_time(now - 60, now), RelativeTime::Minutes(1));
        assert_eq!(relative_time(now - 12 * 60, now), RelativeTime::Minutes(12));
        assert_eq!(relative_time(now - 3_599, now), RelativeTime::Minutes(59));
        assert_eq!(relative_time(now - 3_600, now), RelativeTime::Hours(1));
        assert_eq!(relative_time(now - 2 * 3_600, now), RelativeTime::Hours(2));
        assert_eq!(relative_time(now - 86_399, now), RelativeTime::Hours(23));
        assert_eq!(relative_time(now - 86_400, now), RelativeTime::Days(1));
        assert_eq!(relative_time(now - 3 * 86_400, now), RelativeTime::Days(3));
    }

    #[test]
    fn test_format_relative_time() {
        let dummy_translate = |key: &str| match key {
            "LABEL.PLAYLIST_STATS_TIME_JUST_NOW" => "just now".into(),
            "LABEL.PLAYLIST_STATS_TIME_MINS_AGO" => "{count} min ago".into(),
            "LABEL.PLAYLIST_STATS_TIME_HOURS_AGO" => "{count} h ago".into(),
            "LABEL.PLAYLIST_STATS_TIME_DAYS_AGO" => "{count} d ago".into(),
            _ => key.into(),
        };

        assert_eq!(format_relative_time(RelativeTime::JustNow, &dummy_translate), "just now");
        assert_eq!(format_relative_time(RelativeTime::Minutes(12), &dummy_translate), "12 min ago");
        assert_eq!(format_relative_time(RelativeTime::Hours(2), &dummy_translate), "2 h ago");
        assert_eq!(format_relative_time(RelativeTime::Days(3), &dummy_translate), "3 d ago");
    }

    #[test]
    fn test_classify_target_status() {
        let now_u64: u64 = 1_700_000_000;
        let now_i64 = now_u64 as i64;

        // No data
        assert_eq!(classify_target_status(None, now_i64), TargetStatus::NoData);

        // Failed run
        let failed = make_test_analytics(false, None);
        assert_eq!(classify_target_status(Some(&failed), now_i64), TargetStatus::Error);

        // Success without EPG
        let ok_no_epg = make_test_analytics(true, None);
        assert_eq!(classify_target_status(Some(&ok_no_epg), now_i64), TargetStatus::Ok);

        // Success with healthy EPG (guide lasts 7 days)
        let healthy_epg = EpgAnalytics {
            live_channels: 100,
            mapped: 90,
            guide_until: Some(now_i64 + 7 * 86_400),
            sources: vec![EpgSourceAnalytics {
                input: "p1".into(),
                label: "guide.xml".into(),
                kind: EpgSourceTypeDto::Xmltv,
                mapped_channels: 90,
                guide_days: Some(7),
                fetched_at: Some(now_u64 - 3600),
                failed_at: None,
                error: None,
            }],
            missing_by_group: Vec::new(),
            missing_other: 10,
        };
        let ok_with_epg = make_test_analytics(true, Some(healthy_epg));
        assert_eq!(classify_target_status(Some(&ok_with_epg), now_i64), TargetStatus::Ok);

        // Stale because source failed
        let mut failed_source_epg = ok_with_epg.clone();
        failed_source_epg.epg.as_mut().unwrap().sources[0].failed_at = Some(now_u64 - 1000);
        assert_eq!(classify_target_status(Some(&failed_source_epg), now_i64), TargetStatus::EpgStale);

        // Stale because guide ends in 12 hours (< 24 hours)
        let mut expiring_epg = ok_with_epg.clone();
        expiring_epg.epg.as_mut().unwrap().guide_until = Some(now_i64 + 12 * 3600);
        assert_eq!(classify_target_status(Some(&expiring_epg), now_i64), TargetStatus::EpgStale);
    }

    #[test]
    fn test_t_fmt() {
        let dummy = |key: &str| match key {
            "TEMPLATE" => "Kept {kept} of {total} ({percent}%)".to_string(),
            _ => key.to_string(),
        };

        let formatted = t_fmt(&dummy, "TEMPLATE", &[("kept", "100"), ("total", "200"), ("percent", "50.0")]);
        assert_eq!(formatted, "Kept 100 of 200 (50.0%)");
    }

    #[test]
    fn test_derive_effective_target() {
        let t1 = Rc::new(UiTargetAnalyticsEntry::from(TargetAnalyticsEntryDto {
            target_id: 1,
            name: "Target 1 (No Analytics)".into(),
            outputs: Vec::new(),
            next_scheduled: None,
            analytics: None,
        }));
        let t2 = Rc::new(UiTargetAnalyticsEntry::from(TargetAnalyticsEntryDto {
            target_id: 2,
            name: "Target 2 (Has Analytics)".into(),
            outputs: Vec::new(),
            next_scheduled: None,
            analytics: Some(make_test_analytics(true, None)),
        }));
        let t3 = Rc::new(UiTargetAnalyticsEntry::from(TargetAnalyticsEntryDto {
            target_id: 3,
            name: "Target 3 (Has Analytics)".into(),
            outputs: Vec::new(),
            next_scheduled: None,
            analytics: Some(make_test_analytics(true, None)),
        }));

        let list = vec![t1.clone(), t2.clone(), t3.clone()];

        // 1. User choice exists in list -> returns user choice
        assert_eq!(derive_effective_target(Some(3), &list).map(|t| t.target_id), Some(3));
        assert_eq!(derive_effective_target(Some(1), &list).map(|t| t.target_id), Some(1));

        // 2. User choice not in list -> returns first target with analytics (t2)
        assert_eq!(derive_effective_target(Some(99), &list).map(|t| t.target_id), Some(2));

        // 3. User choice is None -> returns first target with analytics (t2)
        assert_eq!(derive_effective_target(None, &list).map(|t| t.target_id), Some(2));

        // 4. No targets have analytics -> returns first target
        let no_analytics = vec![t1.clone()];
        assert_eq!(derive_effective_target(None, &no_analytics).map(|t| t.target_id), Some(1));

        // 5. Empty list -> returns None
        let empty: &[Rc<UiTargetAnalyticsEntry>] = &[];
        assert_eq!(derive_effective_target(None, empty).map(|t| t.target_id), None);
        assert_eq!(derive_effective_target(Some(1), empty).map(|t| t.target_id), None);
    }

    #[test]
    fn t_fmt_split_separates_the_highlighted_value() {
        let translate = |_: &str| "{kept} of {total} kept".to_string();
        let (before, value, after) = t_fmt_split(&translate, "K", ("kept", "5"), &[("total", "9")]);
        assert_eq!((before.as_str(), value.as_str(), after.as_str()), ("", "5", " of 9 kept"));

        let without = |_: &str| "no placeholder".to_string();
        let (before, value, after) = t_fmt_split(&without, "K", ("kept", "5"), &[]);
        assert_eq!((before.as_str(), value.as_str(), after.as_str()), ("no placeholder", "", ""));
    }
}
