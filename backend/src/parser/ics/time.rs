use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::{Tz, UTC};
use shared::error::TuliproxError;
use std::collections::HashMap;

pub struct ParsedIcsDateTime {
    pub timestamp: i64,
    pub display: String,
    pub display_timezone: Tz,
    pub all_day: bool,
}

pub fn parse_ics_datetime(
    params: &HashMap<String, String>,
    value: &str,
    fallback_timezone: &str,
) -> Result<ParsedIcsDateTime, TuliproxError> {
    if params.get("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) {
        return parse_ics_date(value, fallback_timezone);
    }

    if let Some(stripped) = value.strip_suffix('Z') {
        let naive = parse_naive_datetime(stripped)?;
        let dt = Utc.from_utc_datetime(&naive);
        return Ok(ParsedIcsDateTime {
            timestamp: dt.timestamp(),
            display: dt.to_rfc3339(),
            display_timezone: UTC,
            all_day: false,
        });
    }

    let timezone = params.get("TZID").map_or(fallback_timezone, String::as_str);
    let tz: Tz =
        timezone.parse().map_err(|_| TuliproxError::ConfigEpg(format!("Unknown ICS timezone '{timezone}'")))?;
    let naive = parse_naive_datetime(value)?;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .or_else(|| tz.from_local_datetime(&naive).earliest())
        .ok_or_else(|| TuliproxError::ConfigEpg(format!("Invalid local ICS datetime '{value}' in {timezone}")))?;

    Ok(ParsedIcsDateTime {
        timestamp: local.with_timezone(&Utc).timestamp(),
        display: local.to_rfc3339(),
        display_timezone: tz,
        all_day: false,
    })
}

pub fn display_from_timestamp_in_timezone(timestamp: i64, timezone: Tz) -> Option<String> {
    timezone.timestamp_opt(timestamp, 0).single().map(|dt| dt.to_rfc3339())
}

fn parse_ics_date(value: &str, fallback_timezone: &str) -> Result<ParsedIcsDateTime, TuliproxError> {
    let tz: Tz = fallback_timezone
        .parse()
        .map_err(|_| TuliproxError::ConfigEpg(format!("Unknown ICS timezone '{fallback_timezone}'")))?;
    let date = NaiveDate::parse_from_str(value, "%Y%m%d")
        .map_err(|err| TuliproxError::ConfigEpg(format!("Invalid ICS DATE '{value}': {err}")))?;
    let naive =
        date.and_hms_opt(0, 0, 0).ok_or_else(|| TuliproxError::ConfigEpg(format!("Invalid ICS DATE '{value}'")))?;
    let local =
        tz.from_local_datetime(&naive).single().or_else(|| tz.from_local_datetime(&naive).earliest()).ok_or_else(
            || TuliproxError::ConfigEpg(format!("Invalid local ICS DATE '{value}' in {fallback_timezone}")),
        )?;

    Ok(ParsedIcsDateTime {
        timestamp: local.with_timezone(&Utc).timestamp(),
        display: local.to_rfc3339(),
        display_timezone: tz,
        all_day: true,
    })
}

fn parse_naive_datetime(value: &str) -> Result<NaiveDateTime, TuliproxError> {
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M"))
        .map_err(|err| TuliproxError::ConfigEpg(format!("Invalid ICS datetime '{value}': {err}")))
}

pub fn parse_ics_duration(value: &str) -> Option<i64> {
    let value = value.strip_prefix('P')?;
    if value.is_empty() {
        return None;
    }

    let mut in_time = false;
    let mut saw_time_designator = false;
    let mut saw_time_component = false;
    let mut saw_component = false;
    let mut last_component_order = 0_u8;
    let mut number: Option<i64> = None;
    let mut seconds = 0_i64;

    for ch in value.chars() {
        if ch.is_ascii_digit() {
            let digit = i64::from(ch.to_digit(10)?);
            number = Some(number.unwrap_or_default().checked_mul(10)?.checked_add(digit)?);
            continue;
        }

        if ch == 'T' {
            if in_time || number.is_some() {
                return None;
            }
            in_time = true;
            saw_time_designator = true;
            continue;
        }

        let (component_order, multiplier) = match ch {
            'D' if !in_time => (1, 86_400_i64),
            'H' if in_time => (2, 3_600_i64),
            'M' if in_time => (3, 60_i64),
            'S' if in_time => (4, 1_i64),
            _ => return None,
        };
        if component_order <= last_component_order {
            return None;
        }

        let component = number.take()?.checked_mul(multiplier)?;
        seconds = seconds.checked_add(component)?;
        last_component_order = component_order;
        saw_component = true;
        saw_time_component |= in_time;
    }

    (number.is_none() && saw_component && (!saw_time_designator || saw_time_component) && seconds > 0)
        .then_some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(items: &[(&str, &str)]) -> HashMap<String, String> {
        items.iter().map(|(key, value)| ((*key).to_string(), (*value).to_string())).collect()
    }

    #[test]
    fn parses_utc_datetime() {
        let parsed = parse_ics_datetime(&HashMap::new(), "20260306T123000Z", "Europe/Berlin").expect("datetime");
        assert_eq!(parsed.timestamp, 1_772_800_200);
        assert!(!parsed.all_day);
    }

    #[test]
    fn parses_timezone_datetime() {
        let parsed =
            parse_ics_datetime(&params(&[("TZID", "Europe/Berlin")]), "20260306T123000", "UTC").expect("datetime");
        assert_eq!(parsed.timestamp, 1_772_796_600);
        assert_eq!(parsed.display, "2026-03-06T12:30:00+01:00");
    }

    #[test]
    fn parses_floating_datetime_with_fallback_timezone() {
        let parsed = parse_ics_datetime(&HashMap::new(), "20260306T123000", "Europe/Berlin").expect("datetime");
        assert_eq!(parsed.timestamp, 1_772_796_600);
    }

    #[test]
    fn parses_duration() {
        assert_eq!(parse_ics_duration("PT1H30M"), Some(5_400));
        assert_eq!(parse_ics_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_ics_duration("-PT1H"), None);
    }

    #[test]
    fn rejects_incomplete_and_non_positive_durations() {
        for duration in ["PT1H2", "P1D2", "PT", "P", "PT0S", "-PT1H"] {
            assert_eq!(parse_ics_duration(duration), None, "duration {duration}");
        }
    }

    #[test]
    fn rejects_duration_component_and_sum_overflow() {
        assert_eq!(parse_ics_duration(&format!("P{}D", i64::MAX)), None);
        assert_eq!(parse_ics_duration(&format!("P1DT{}S", i64::MAX)), None);
        assert_eq!(parse_ics_duration("PT999999999999999999999999999999999999S"), None);
    }

    #[test]
    fn marks_value_date_as_all_day() {
        let parsed = parse_ics_datetime(&params(&[("VALUE", "DATE")]), "20260306", "UTC").expect("date");
        assert!(parsed.all_day);
    }
}
