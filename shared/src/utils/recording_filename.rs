//! DVR recording filename rendering.
//!
//! Two responsibilities:
//! 1. **Template validation** — only the documented placeholders are
//!    accepted; malformed braces and unknown names are rejected at
//!    configuration load.
//! 2. **Stem rendering** — given a validated template, a [`RecordingFilenameContext`]
//!    and an IANA timezone, produce a sanitized stem that is safe to
//!    use as a file name on the target platform. The stem is capped at
//!    240 UTF-8 bytes without splitting a code point; if the rendered
//!    content is empty after sanitization, the context's `task_id` is
//!    used as a fallback.

use chrono::{Datelike, TimeZone, Timelike};
use std::fmt;

/// Allowed placeholders in the recording filename template.
/// Order matters for stable error messages.
pub const RECORDING_FILENAME_PLACEHOLDERS: &[&str] =
    &["{channel}", "{program_title}", "{start_time}", "{end_time}", "{episode}", "{owner}"];

/// Maximum final-stem length in UTF-8 bytes. The cap leaves room for any
/// suffix and a small extension while staying well under common filesystem
/// limits.
pub const MAX_RECORDING_STEM_BYTES: usize = 240;

/// Default episode segment produced when both `season` and `episode` are set.
pub const EPISODE_PREFIX: &str = "S";
pub const EPISODE_INFIX: &str = "E";
pub const EPISODE_PAD: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFilenameContext {
    pub task_id: String,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub program_title: Option<String>,
    pub episode_season: Option<u32>,
    pub episode_number: Option<u32>,
    pub owner_display: Option<String>,
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    pub scheduled_start: Option<i64>,
    pub scheduled_end: Option<i64>,
}

impl RecordingFilenameContext {
    /// Look up the rendered value for a placeholder. Returns `None` when
    /// the context has no value for the requested field.
    pub fn value_for(&self, placeholder: &str) -> Option<String> {
        match placeholder {
            "{channel}" => self.channel_name.clone().or_else(|| self.channel_id.clone()),
            "{program_title}" => self.program_title.clone(),
            "{start_time}" => None, // Time rendering needs the timezone; see `render_recording_stem`.
            "{end_time}" => None,
            "{episode}" => self.render_episode_segment(),
            "{owner}" => self.owner_display.clone(),
            _ => None,
        }
    }

    fn render_episode_segment(&self) -> Option<String> {
        match (self.episode_season, self.episode_number) {
            (Some(s), Some(e)) => Some(format!("{EPISODE_PREFIX}{s:0EPISODE_PAD$}{EPISODE_INFIX}{e:0EPISODE_PAD$}")),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordingFilenameError {
    #[error("recording filename template is empty")]
    Empty,
    #[error("recording filename exceeds {MAX_RECORDING_STEM_BYTES} bytes (got {bytes})")]
    TooLong { bytes: usize },
    #[error("unknown placeholder '{0}' in recording filename template")]
    UnknownPlaceholder(String),
    #[error("recording filename template has an unmatched '{{'")]
    UnmatchedOpenBrace,
    #[error("recording filename template has an unmatched '}}'")]
    UnmatchedCloseBrace,
    #[error("recording filename template must contain at least one placeholder")]
    NoPlaceholder,
    /// Template contains a literal path separator (`/` or `\`). Path
    /// separators must never reach the rendered stem — they would
    /// escape the recording directory or collide with the partial-file
    /// suffix machinery.
    #[error("recording filename template contains path separator '{0}'")]
    PathSeparator(char),
    #[error("recording timestamp {0} is out of range for the configured timezone")]
    TimeOutOfRange(String),
}

/// Validate a recording filename template. Returns `Ok(())` when the
/// template is acceptable.
pub fn validate_recording_template(template: &str) -> Result<(), RecordingFilenameError> {
    if template.is_empty() {
        return Err(RecordingFilenameError::Empty);
    }
    if template.len() > MAX_RECORDING_STEM_BYTES {
        return Err(RecordingFilenameError::TooLong { bytes: template.len() });
    }
    let bytes = template.as_bytes();
    let mut i = 0;
    let mut found_placeholder = false;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close_offset) = template[i + 1..].find('}') {
                let end = i + 1 + close_offset;
                let placeholder = &template[i..=end];
                if !RECORDING_FILENAME_PLACEHOLDERS.contains(&placeholder) {
                    return Err(RecordingFilenameError::UnknownPlaceholder(placeholder.to_string()));
                }
                found_placeholder = true;
                i = end + 1;
            } else {
                return Err(RecordingFilenameError::UnmatchedOpenBrace);
            }
        } else if bytes[i] == b'}' {
            return Err(RecordingFilenameError::UnmatchedCloseBrace);
        } else if bytes[i] == b'/' || bytes[i] == b'\\' {
            // Reject literal path separators. Sanitizing instead would
            // collapse templates that the operator intended to be
            // separate (e.g. `{channel}/{program_title}`) into a single
            // filename component; failing closed is the safer default.
            return Err(RecordingFilenameError::PathSeparator(bytes[i] as char));
        } else {
            i += 1;
        }
    }
    if !found_placeholder {
        return Err(RecordingFilenameError::NoPlaceholder);
    }
    Ok(())
}

/// Render a recording filename stem. The template must already be
/// validated. Returns the sanitized stem or an error when the rendering
/// fails (e.g., out-of-range timestamps). When the rendered content is
/// empty, `ctx.task_id` is used as a stable fallback.
pub fn render_recording_stem<TzLike>(
    template: &str,
    ctx: &RecordingFilenameContext,
    tz: &TzLike,
) -> Result<String, RecordingFilenameError>
where
    TzLike: TimeZone,
    TzLike::Offset: fmt::Display,
{
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close_offset) = template[i + 1..].find('}') {
                let end = i + 1 + close_offset;
                let placeholder = &template[i..=end];
                let value = match placeholder {
                    "{start_time}" => render_time(ctx.program_start.or(ctx.scheduled_start), tz)?.unwrap_or_default(),
                    "{end_time}" => render_time(ctx.program_end.or(ctx.scheduled_end), tz)?.unwrap_or_default(),
                    other => ctx.value_for(other).unwrap_or_default(),
                };
                out.push_str(&sanitize_filename_segment(&value));
                i = end + 1;
            } else {
                return Err(RecordingFilenameError::UnmatchedOpenBrace);
            }
        } else if bytes[i] == b'}' {
            return Err(RecordingFilenameError::UnmatchedCloseBrace);
        } else {
            out.push(template[i..].chars().next().expect("byte-aligned utf-8"));
            i += template[i..].chars().next().expect("byte-aligned utf-8").len_utf8();
        }
    }
    let collapsed = collapse_empty_components(&out);
    if collapsed.is_empty() {
        return Ok(ctx.task_id.clone());
    }
    cap_at_byte_boundary(&collapsed, MAX_RECORDING_STEM_BYTES)
}

/// Render an `i64` Unix timestamp as `YYYY-MM-DD_HH-mm` in the supplied
/// timezone. Returns `Err(TimeOutOfRange)` when the timestamp cannot be
/// converted to a date in the timezone.
fn render_time<TzLike>(timestamp: Option<i64>, tz: &TzLike) -> Result<Option<String>, RecordingFilenameError>
where
    TzLike: TimeZone,
    TzLike::Offset: fmt::Display,
{
    let Some(ts) = timestamp else { return Ok(None) };
    let local = match tz.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return Err(RecordingFilenameError::TimeOutOfRange(ts.to_string())),
    };
    let formatted = format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute()
    );
    Ok(Some(formatted))
}

/// Sanitize a single filename segment with the shared filename allow-list:
/// letters of every script are kept, whitespace becomes `_`, symbols and
/// separators are dropped.
fn sanitize_filename_segment(s: &str) -> String { crate::utils::sanitize_filename_chars(s, true) }

/// Collapse runs of `_` / `.` into one separator and trim leading/trailing
/// ones so empty fields do not produce dangling separators. A run that
/// contains a `.` stays a `.`, so dots in titles and literal extensions
/// (`{program_title}.ts`) survive.
fn collapse_empty_components(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending: Option<char> = None;
    for ch in s.chars() {
        if matches!(ch, '_' | '.') {
            if pending != Some('.') {
                pending = Some(ch);
            }
        } else {
            if let Some(sep) = pending.take() {
                if !out.is_empty() {
                    out.push(sep);
                }
            }
            out.push(ch);
        }
    }
    out
}

/// Cap a string at `max_bytes` UTF-8 bytes without splitting a code
/// point. Preserves the suffix if the stem is already short enough.
pub fn cap_at_byte_boundary(s: &str, max_bytes: usize) -> Result<String, RecordingFilenameError> {
    if s.len() <= max_bytes {
        return Ok(s.to_string());
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return Err(RecordingFilenameError::TooLong { bytes: s.len() });
    }
    Ok(s[..end].to_string())
}

/// Numbered collision suffix appended to a stem that collides with an
/// existing path. Mirrors the legacy `RecordingTask::new` behavior of
/// appending `_<n>` before the extension.
pub fn next_collision_suffix(stem: &str, existing_paths: &[String]) -> String {
    let mut counter: u32 = 1;
    loop {
        let candidate = format!("{stem}_{counter}");
        if !existing_paths.iter().any(|p| p == &candidate) {
            return candidate;
        }
        counter = counter.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;

    fn empty_ctx() -> RecordingFilenameContext {
        RecordingFilenameContext {
            task_id: "task-fallback".to_string(),
            channel_id: None,
            channel_name: None,
            program_title: None,
            episode_season: None,
            episode_number: None,
            owner_display: None,
            program_start: Some(0),
            program_end: Some(0),
            scheduled_start: Some(0),
            scheduled_end: Some(0),
        }
    }

    fn utc() -> Tz { Tz::UTC }

    #[test]
    fn validation_accepts_canonical_template() {
        validate_recording_template("{channel}_{program_title}_{start_time}").expect("accept");
    }

    #[test]
    fn validation_rejects_unknown_placeholder() {
        let err = validate_recording_template("{channel}_{nope}").unwrap_err();
        assert!(matches!(err, RecordingFilenameError::UnknownPlaceholder(_)));
    }

    #[test]
    fn validation_rejects_unbalanced_braces() {
        assert!(matches!(
            validate_recording_template("{channel").unwrap_err(),
            RecordingFilenameError::UnmatchedOpenBrace
        ));
        assert!(matches!(
            validate_recording_template("channel}_end").unwrap_err(),
            RecordingFilenameError::UnmatchedCloseBrace
        ));
    }

    #[test]
    fn validation_rejects_empty_template() {
        assert!(matches!(validate_recording_template("").unwrap_err(), RecordingFilenameError::Empty));
    }

    #[test]
    fn validation_rejects_template_without_any_placeholder() {
        assert!(matches!(
            validate_recording_template("static_name").unwrap_err(),
            RecordingFilenameError::NoPlaceholder
        ));
    }

    #[test]
    fn rendering_falls_back_to_task_id_when_empty() {
        let ctx = empty_ctx();
        let stem = render_recording_stem("{program_title}", &ctx, &utc()).expect("render");
        assert_eq!(stem, "task-fallback");
    }

    #[test]
    fn rendering_drops_symbols() {
        let mut ctx = empty_ctx();
        ctx.program_title = Some("Café — 90° 🎬".to_string());
        let stem = render_recording_stem("{program_title}", &ctx, &utc()).expect("render");
        assert_eq!(stem, "Café_90");
    }

    #[test]
    fn rendering_keeps_letters_of_every_script() {
        for (title, expected) in [
            ("Tatort: Münster", "Tatort_Münster"),
            ("Çalıkuşu", "Çalıkuşu"),
            ("Новости дня", "Новости_дня"),
            ("نشرة الأخبار", "نشرة_الأخبار"),
        ] {
            let mut ctx = empty_ctx();
            ctx.program_title = Some(title.to_string());
            let stem = render_recording_stem("{program_title}", &ctx, &utc()).expect("render");
            assert_eq!(stem, expected);
        }
    }

    #[test]
    fn rendering_renders_time_in_configured_timezone() {
        let mut ctx = empty_ctx();
        // 1700000000 = 2023-11-14 22:13:20 UTC
        ctx.program_start = Some(1_700_000_000);
        let berlin = Tz::Europe__Berlin;
        let stem = render_recording_stem("{start_time}", &ctx, &berlin).expect("render");
        assert_eq!(stem, "2023-11-14_23-13");
    }

    #[test]
    fn rendering_renders_episode_only_when_both_present() {
        let mut ctx = empty_ctx();
        ctx.episode_season = Some(2);
        ctx.episode_number = Some(7);
        let stem = render_recording_stem("{episode}", &ctx, &utc()).expect("render");
        assert_eq!(stem, "S02E07");

        let mut partial = empty_ctx();
        partial.episode_season = Some(2);
        let stem = render_recording_stem("before_{episode}_after", &partial, &utc()).expect("render");
        // Both separators collapse into a single `_`, so the dangling
        // segments around the empty {episode} are absorbed.
        assert!(!stem.contains("__"));
    }

    #[test]
    fn rendering_preserves_owner_sanitized_text() {
        let mut ctx = empty_ctx();
        ctx.owner_display = Some("alice@example.com".to_string());
        let stem = render_recording_stem("{owner}", &ctx, &utc()).expect("render");
        // `@` and `.` are kept by the sanitizer.
        assert_eq!(stem, "alice@example.com");
    }

    #[test]
    fn rendering_caps_at_byte_boundary_without_splitting_code_point() {
        let mut ctx = empty_ctx();
        // 250 ASCII chars exceeds the 240 cap.
        ctx.program_title = Some("a".repeat(250));
        let stem = render_recording_stem("{program_title}", &ctx, &utc()).expect("render");
        assert!(stem.len() <= MAX_RECORDING_STEM_BYTES);
        // No truncation marker — the cap is silent.
    }

    #[test]
    fn rendering_caps_at_utf8_boundary_for_multibyte_content() {
        // Two-byte letters survive the sanitizer, so the cap must cut on a
        // character boundary.
        let mut ctx = empty_ctx();
        ctx.program_title = Some("Ж".repeat(300));
        let stem = render_recording_stem("{program_title}", &ctx, &utc()).expect("render");
        assert!(stem.len() <= MAX_RECORDING_STEM_BYTES);
        assert!(stem.chars().all(|c| c == 'Ж'), "{stem}");
    }

    #[test]
    fn cap_at_byte_boundary_handles_multibyte_string_without_splitting() {
        // 5 × 2-byte char = 10 bytes. Cap at 7 should walk back to 6
        // (the second char boundary), giving 3 chars.
        let s = "ÖÖÖÖÖ";
        let capped = cap_at_byte_boundary(s, 7).expect("cap");
        assert_eq!(capped, "ÖÖÖ");
        assert!(capped.len() <= 7);
    }

    #[test]
    fn collapse_empty_components_drops_consecutive_separators() {
        assert_eq!(collapse_empty_components("a__b___c"), "a_b_c");
        assert_eq!(collapse_empty_components("_leading"), "leading");
        assert_eq!(collapse_empty_components("trailing__"), "trailing");
        assert_eq!(collapse_empty_components("a..b"), "a.b");
        assert_eq!(collapse_empty_components("Title_.ts"), "Title.ts");
        assert_eq!(collapse_empty_components("Mr._Robot"), "Mr.Robot");
        assert_eq!(collapse_empty_components(".hidden."), "hidden");
    }

    #[test]
    fn rendering_keeps_dots_and_literal_extensions() {
        let mut ctx = empty_ctx();
        ctx.program_title = Some("Mr. Robot".to_string());
        assert_eq!(render_recording_stem("{program_title}.ts", &ctx, &utc()).expect("render"), "Mr.Robot.ts");
        ctx.program_title = Some("Doku 2.0".to_string());
        assert_eq!(render_recording_stem("{program_title}", &ctx, &utc()).expect("render"), "Doku_2.0");
    }

    #[test]
    fn collision_suffix_appends_counter() {
        let stem = "pilot";
        let existing = vec!["pilot".to_string(), "pilot_1".to_string(), "other".to_string()];
        let next = next_collision_suffix(stem, &existing);
        assert_eq!(next, "pilot_2");
    }

    #[test]
    fn collision_suffix_returns_stem_when_no_collision() {
        let next = next_collision_suffix("pilot", &[]);
        assert_eq!(next, "pilot_1");
    }

    #[test]
    fn cap_at_byte_boundary_handles_already_short_string() {
        assert_eq!(cap_at_byte_boundary("hello", 10).unwrap(), "hello");
    }

    #[test]
    fn cap_at_byte_boundary_truncates_exactly_at_max() {
        let s = "a".repeat(250);
        let capped = cap_at_byte_boundary(&s, 240).unwrap();
        assert_eq!(capped.len(), 240);
    }
}
