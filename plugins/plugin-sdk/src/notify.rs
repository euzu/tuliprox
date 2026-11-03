//! Helpers every notification plugin needs, so each one does not carry its own copy.
//!
//! * [`delivery_for_status`] turns an HTTP status (and a `Retry-After` header) into the
//!   [`DeliveryError`] the host's outbox understands.
//! * [`parse_retry_after`] reads a `Retry-After` header in either legal form.
//! * [`iso8601`] writes a Unix time the way chat APIs read a timestamp.
//! * [`Section`] and [`sections_of`] read the event's channel-neutral `sections` out of a notification's `payload`.
//! * [`markdown`] turns plain text and sections into escaped markdown, for services that render it.
//!
//! # Status mapping
//!
//! | Status | Result |
//! | --- | --- |
//! | 2xx | `Ok` |
//! | 429, 408 | `retry-after` when `Retry-After` is usable, else `transient` |
//! | 5xx except 501 and 505 | `transient` |
//! | 501, 505, 1xx, 3xx, any other 4xx | `permanent` |
//!
//! `Retry-After` is read as delta-seconds or as an HTTP date (RFC 2822 form, a past date
//! meaning "now") and capped at 24 hours. A reason never contains anything but the status.

use crate::{clock, wit::notification_hooks::DeliveryError};
use serde::Deserialize;

/// No provider may park a retry for longer than this.
const MAX_RETRY_AFTER_SECS: u64 = 24 * 60 * 60;

/// [`delivery_for_status_at`] at the host clock's time.
pub fn delivery_for_status(status: u16, retry_after: Option<&str>) -> Result<(), DeliveryError> {
    delivery_for_status_at(status, retry_after, clock::now_unix())
}

/// How an HTTP status ends one delivery attempt. `now` is unix seconds, for an
/// HTTP-date `Retry-After`.
pub fn delivery_for_status_at(status: u16, retry_after: Option<&str>, now: i64) -> Result<(), DeliveryError> {
    match status {
        200..=299 => Ok(()),
        429 | 408 => Err(match retry_after.and_then(|raw| parse_retry_after(raw, now)) {
            Some(secs) => DeliveryError::RetryAfter(secs),
            None => DeliveryError::Transient(format!("status {status}")),
        }),
        // 501 and 505 say the endpoint will never take this request.
        501 | 505 => Err(DeliveryError::Permanent(format!("status {status}"))),
        500..=599 => Err(DeliveryError::Transient(format!("status {status}"))),
        _ => Err(DeliveryError::Permanent(format!("status {status}"))),
    }
}

/// A `Retry-After` header in either legal form, in seconds, at most 24 hours.
pub fn parse_retry_after(raw: &str, now: i64) -> Option<u32> {
    let raw = raw.trim();
    let secs = match raw.parse::<u64>() {
        Ok(secs) => secs,
        // A date already in the past means the wait is over: retry now.
        Err(_) => u64::try_from(parse_http_date(raw)?.saturating_sub(now).max(0)).unwrap_or(0),
    };
    u32::try_from(secs.min(MAX_RETRY_AFTER_SECS)).ok()
}

/// An RFC 2822 date (`Sun, 06 Nov 1994 08:49:37 GMT`) as unix seconds. The
/// weekday is optional and not checked; the zone is `GMT`, `UT`, `UTC`, `Z`, a
/// North American zone name or `±hhmm`. Obsolete HTTP forms (RFC 850,
/// `asctime`) are not read, exactly as the built-in channel does not read them.
fn parse_http_date(raw: &str) -> Option<i64> {
    let mut parts = raw.split_whitespace();
    let mut first = parts.next()?;
    if first.ends_with(',') {
        first = parts.next()?;
    }
    let day: i64 = first.parse().ok().filter(|day| (1..=31).contains(day))?;
    let month = month_of(parts.next()?)?;
    let year_text = parts.next()?;
    let year = match (year_text.len(), year_text.parse::<i64>().ok()?) {
        (2, year) if year < 50 => 2000 + year,
        (2 | 3, year) => 1900 + year,
        (_, year) => year,
    };
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok().filter(|hour| (0..24).contains(hour))?;
    let minute: i64 = clock.next()?.parse().ok().filter(|minute| (0..60).contains(minute))?;
    let second: i64 = match clock.next() {
        Some(second) => second.parse().ok().filter(|second| (0..61).contains(second))?,
        None => 0,
    };
    if clock.next().is_some() {
        return None;
    }
    let offset = zone_offset(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

fn month_of(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let name = name.to_ascii_lowercase();
    MONTHS.iter().position(|month| *month == name).and_then(|at| i64::try_from(at + 1).ok())
}

/// The zone's offset east of UTC, in seconds.
fn zone_offset(zone: &str) -> Option<i64> {
    let named_hours = match zone.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" => Some(0),
        "EDT" => Some(-4),
        "EST" | "CDT" => Some(-5),
        "CST" | "MDT" => Some(-6),
        "MST" | "PDT" => Some(-7),
        "PST" => Some(-8),
        _ => None,
    };
    if let Some(hours) = named_hours {
        return Some(hours * 3_600);
    }
    let (sign, digits) = match zone.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`, as most chat APIs read a time. `None`
/// outside the years 0000-9999.
pub fn iso8601(unix: i64) -> Option<String> {
    let days = unix.div_euclid(86_400);
    let rest = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60))
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`, the inverse of [`days_from_civil`]).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// A titled block of plain-text lines of an event (an entry of `sections` in the
/// notification's `payload`). It carries no markup; a plugin lays it out in the
/// format of its service.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Section {
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub lines: Vec<String>,
}

/// The `sections` of a notification's `payload` (the event as JSON); empty when
/// the event has none or the payload is not JSON.
#[must_use]
pub fn sections_of(payload: &str) -> Vec<Section> {
    #[derive(Deserialize, Default)]
    struct Detail {
        #[serde(default)]
        sections: Vec<Section>,
    }
    serde_json::from_str::<Detail>(payload).unwrap_or_default().sections
}

/// Markdown for services that render it (Gotify, ntfy, Matrix).
pub mod markdown {
    use super::Section;

    /// One line of plain text as a markdown line: the characters markdown reads
    /// are escaped, and a leading `>`, `#`, `-`, `+` or `1.` is too, so a line can
    /// never become a quote, a heading or a list.
    pub fn escape_line(line: &str) -> String {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let mut out = String::from(indent);
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        let numbered = digits > 0 && matches!(trimmed[digits..].chars().next(), Some('.' | ')'));
        for (at, ch) in trimmed.chars().enumerate() {
            let leading_marker = at == 0 && matches!(ch, '>' | '#' | '-' | '+');
            let numbered_marker = numbered && at == digits;
            if matches!(ch, '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']' | '<' | '&')
                || leading_marker
                || numbered_marker
            {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    /// Plain text lines as markdown lines with hard breaks, which markdown needs to
    /// keep one line per line.
    pub fn lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
        lines.map(escape_line).collect::<Vec<_>>().join("  \n")
    }

    /// The sections as markdown: a bold heading and its lines, a blank line between sections.
    pub fn sections(sections: &[Section]) -> Option<String> {
        let blocks: Vec<String> = sections
            .iter()
            .filter_map(|section| {
                let kept: Vec<&str> =
                    section.lines.iter().map(|line| line.trim_end()).filter(|line| !line.trim().is_empty()).collect();
                if kept.is_empty() {
                    return None;
                }
                let heading = section.heading.trim();
                let body = lines(kept.into_iter());
                Some(if heading.is_empty() { body } else { format!("**{}**  \n{body}", escape_line(heading)) })
            })
            .collect();
        (!blocks.is_empty()).then(|| blocks.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(outcome: &Result<(), DeliveryError>) -> String {
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(DeliveryError::Transient(_)) => "transient".to_string(),
            Err(DeliveryError::Permanent(_)) => "permanent".to_string(),
            Err(DeliveryError::RetryAfter(secs)) => format!("retry-after:{secs}"),
        }
    }

    #[test]
    fn every_status_maps_onto_a_delivery() {
        let cases = [
            (200, "ok"),
            (204, "ok"),
            (299, "ok"),
            (100, "permanent"),
            (301, "permanent"),
            (302, "permanent"),
            (400, "permanent"),
            (401, "permanent"),
            (403, "permanent"),
            (404, "permanent"),
            (408, "transient"),
            (429, "transient"),
            (500, "transient"),
            (501, "permanent"),
            (502, "transient"),
            (503, "transient"),
            (504, "transient"),
            (505, "permanent"),
            (599, "transient"),
        ];
        for (status, want) in cases {
            assert_eq!(shape(&delivery_for_status_at(status, None, 0)), want, "status {status}");
        }
    }

    #[test]
    fn retry_after_is_honoured_on_429_and_408_only() {
        assert_eq!(shape(&delivery_for_status_at(429, Some("7"), 0)), "retry-after:7");
        assert_eq!(shape(&delivery_for_status_at(408, Some(" 12 "), 0)), "retry-after:12");
        assert_eq!(shape(&delivery_for_status_at(429, Some("0"), 0)), "retry-after:0");
        assert_eq!(shape(&delivery_for_status_at(503, Some("7"), 0)), "transient");
        assert_eq!(shape(&delivery_for_status_at(404, Some("7"), 0)), "permanent");
        assert_eq!(shape(&delivery_for_status_at(429, Some("soon"), 0)), "transient");
        assert_eq!(shape(&delivery_for_status_at(429, Some("5.5"), 0)), "transient");
        assert_eq!(shape(&delivery_for_status_at(429, Some(""), 0)), "transient");
    }

    #[test]
    fn retry_after_in_http_date_form_is_relative_to_now() {
        // Sun, 06 Nov 1994 08:49:37 GMT
        let at = 784_111_777;
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        assert_eq!(parse_http_date(date), Some(at));
        assert_eq!(parse_retry_after(date, at - 90), Some(90));
        assert_eq!(parse_retry_after(date, at), Some(0));
        assert_eq!(parse_retry_after(date, at + 3_600), Some(0), "a past date means retry now");
        assert_eq!(shape(&delivery_for_status_at(429, Some(date), at - 30)), "retry-after:30");
    }

    #[test]
    fn retry_after_is_capped_at_24_hours_in_both_forms() {
        assert_eq!(parse_retry_after("86400", 0), Some(86_400));
        assert_eq!(parse_retry_after("86401", 0), Some(86_400));
        assert_eq!(parse_retry_after("99999999999999", 0), Some(86_400));
        assert_eq!(parse_retry_after("18446744073709551615", 0), Some(86_400));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777 - 10 * 86_400), Some(86_400));
    }

    #[test]
    fn rfc_2822_dates_are_read_the_way_the_host_reads_them() {
        let at = 784_111_777;
        for (raw, want) in [
            ("06 Nov 1994 08:49:37 GMT", Some(at)),
            ("Sun, 06 Nov 1994 08:49:37 +0000", Some(at)),
            ("Sun, 06 Nov 1994 10:49:37 +0200", Some(at)),
            ("Sun, 06 Nov 1994 03:49:37 EST", Some(at)),
            ("Sun, 06 Nov 1994 08:49 UT", Some(at - 37)),
            ("Sun, 06 Nov 94 08:49:37 GMT", Some(at)),
            ("Sunday, 06-Nov-94 08:49:37 GMT", None),
            ("Sun Nov  6 08:49:37 1994", None),
            ("Sun, 32 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Foo 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 08:49:37", None),
            ("Sun, 06 Nov 1994 08:49:37 GMT extra", None),
            ("", None),
        ] {
            assert_eq!(parse_http_date(raw), want, "{raw:?}");
        }
        // 2000-02-29 exists, 1970-01-01 is the epoch.
        assert_eq!(parse_http_date("01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("29 Feb 2000 00:00:00 GMT"), Some(951_782_400));
    }

    #[test]
    fn dates_are_written_as_utc_iso_8601() {
        assert_eq!(iso8601(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(iso8601(784_111_777).as_deref(), Some("1994-11-06T08:49:37Z"));
        assert_eq!(iso8601(951_782_400).as_deref(), Some("2000-02-29T00:00:00Z"));
        assert_eq!(iso8601(-86_400).as_deref(), Some("1969-12-31T00:00:00Z"));
        assert_eq!(iso8601(i64::MAX), None);
    }

    #[test]
    fn markdown_characters_and_line_start_markers_are_escaped() {
        assert_eq!(
            markdown::escape_line("my_list *x* ~y~ `z` |w| [a] <b> a&b"),
            "my\\_list \\*x\\* \\~y\\~ \\`z\\` \\|w\\| \\[a\\] \\<b> a\\&b"
        );
        assert_eq!(markdown::escape_line("> quote"), "\\> quote");
        assert_eq!(markdown::escape_line("# head"), "\\# head");
        assert_eq!(markdown::escape_line("- item"), "\\- item");
        assert_eq!(markdown::escape_line("+ item"), "\\+ item");
        assert_eq!(markdown::escape_line("1. first"), "1\\. first");
        assert_eq!(markdown::escape_line("12) first"), "12\\) first");
        assert_eq!(markdown::escape_line("  > indented"), "  \\> indented");
        // Mid-line they are ordinary text, so `a -> b` stays readable.
        assert_eq!(markdown::escape_line("a -> b # c - d 1. e"), "a -> b # c - d 1. e");
        assert_eq!(markdown::escape_line("v1.2 released"), "v1.2 released");
    }

    #[test]
    fn sections_become_bold_headings_with_hard_line_breaks_and_a_blank_line_between() {
        let sections = sections_of(
            r#"{"sections":[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels"]},{"heading":"Targets","lines":["main: ok"]}]}"#,
        );
        assert_eq!(
            markdown::sections(&sections).as_deref(),
            Some("**Sources**  \ndemo-in (M3u)  \n1/2 -> 1/2 groups/channels\n\n**Targets**  \nmain: ok")
        );
        assert_eq!(markdown::lines("first\nsecond".lines()), "first  \nsecond");
    }

    #[test]
    fn a_heading_is_escaped_with_the_same_rules_and_an_empty_one_is_left_out() {
        let sections = vec![
            Section { heading: "R_D *team*".to_string(), lines: vec!["ok".to_string()] },
            Section { heading: "Empty".to_string(), lines: vec![] },
            Section { heading: "Blank".to_string(), lines: vec!["  ".to_string()] },
            Section { heading: " ".to_string(), lines: vec!["kept".to_string()] },
        ];
        assert_eq!(markdown::sections(&sections).as_deref(), Some("**R\\_D \\*team\\***  \nok\n\nkept"));
        assert_eq!(markdown::sections(&sections[1..3]), None, "nothing to show");
    }

    #[test]
    fn the_sections_of_a_payload_are_read_and_anything_else_is_empty() {
        assert_eq!(sections_of(r#"{"id":"x","sections":[{"heading":"H","lines":["l"]}]}"#).len(), 1);
        assert_eq!(sections_of("{}"), Vec::<Section>::new());
        assert_eq!(sections_of("not json"), Vec::<Section>::new());
        assert_eq!(sections_of(r#"{"sections":"nope"}"#), Vec::<Section>::new());
    }
}
