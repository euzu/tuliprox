use crate::{
    model::IcsEpgSourceConfig,
    parser::ics::time::{display_from_timestamp_in_timezone, parse_ics_datetime, parse_ics_duration},
};
use chrono_tz::Tz;
use log::warn;
use shared::{
    defaults::{MAX_ICS_DESCRIPTION_LENGTH, MAX_ICS_LINE_LENGTH, MAX_ICS_PROPERTIES_PER_EVENT, MAX_ICS_SUMMARY_LENGTH},
    error::TuliproxError,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct IcsEvent {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub categories: Vec<String>,
    pub start: Option<i64>,
    pub stop: Option<i64>,
    pub start_display: Option<String>,
    pub stop_display: Option<String>,
    pub cancelled: bool,
    pub unsupported_recurrence: bool,
}

#[derive(Debug, Clone)]
pub struct IcsProperty {
    pub name: String,
    pub params: IcsPropertyParams,
    pub value: String,
}

pub type IcsPropertyParams = HashMap<String, String>;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum CalendarState {
    Before,
    Inside,
    After,
}

pub fn parse_ics_events(content: &str, config: &IcsEpgSourceConfig) -> Result<Vec<IcsEvent>, TuliproxError> {
    let lines = unfold_ics_lines(content)?;
    let mut events = Vec::new();
    let mut current = Vec::<String>::new();
    let mut calendar_state = CalendarState::Before;
    let mut event_depth = 0usize;
    let mut collect_current = false;
    let mut current_malformed = false;
    let mut examined_events = 0usize;
    let mut invalid_events = 0usize;
    let mut unsupported_recurrence_events = 0usize;
    let mut event_limit_reached = false;

    for line in lines {
        if is_component_boundary(&line, "BEGIN", "VCALENDAR") {
            if calendar_state != CalendarState::Before {
                return Err(invalid_calendar_structure("nested or multiple BEGIN:VCALENDAR"));
            }
            calendar_state = CalendarState::Inside;
            continue;
        }

        if is_component_boundary(&line, "END", "VCALENDAR") {
            if calendar_state != CalendarState::Inside {
                return Err(invalid_calendar_structure("END:VCALENDAR without matching BEGIN:VCALENDAR"));
            }
            if event_depth != 0 {
                return Err(invalid_calendar_structure("END:VCALENDAR before END:VEVENT"));
            }
            calendar_state = CalendarState::After;
            continue;
        }

        if line.trim().is_empty() && event_depth == 0 {
            continue;
        }

        match calendar_state {
            CalendarState::Before => {
                return Err(invalid_calendar_structure("content before BEGIN:VCALENDAR"));
            }
            CalendarState::After => {
                return Err(invalid_calendar_structure("content after END:VCALENDAR"));
            }
            CalendarState::Inside => {}
        }

        if is_component_boundary(&line, "BEGIN", "VEVENT") {
            let within_budget = examined_events < config.max_events;
            if within_budget {
                examined_events += 1;
            } else {
                event_limit_reached = true;
            }

            if event_depth == 0 {
                current.clear();
                current_malformed = false;
                collect_current = within_budget;
            } else {
                current_malformed |= collect_current;
                if !within_budget {
                    collect_current = false;
                    current.clear();
                }
            }
            event_depth += 1;
            continue;
        }

        if is_component_boundary(&line, "END", "VEVENT") {
            if event_depth == 0 {
                invalid_events += 1;
                continue;
            }

            event_depth -= 1;
            if event_depth == 0 {
                if collect_current {
                    if current_malformed {
                        invalid_events += 1;
                    } else {
                        collect_parsed_event(
                            &current,
                            config,
                            &mut events,
                            &mut invalid_events,
                            &mut unsupported_recurrence_events,
                        );
                    }
                }
                collect_current = false;
                current_malformed = false;
                current.clear();
            }
            continue;
        }

        if event_depth != 0 && collect_current && !current_malformed {
            current.push(line);
        }
    }

    validate_complete_calendar(calendar_state)?;
    log_parse_summary(invalid_events, unsupported_recurrence_events, examined_events, event_limit_reached, config.max_events);

    Ok(events)
}

fn collect_parsed_event(
    lines: &[String],
    config: &IcsEpgSourceConfig,
    events: &mut Vec<IcsEvent>,
    invalid_events: &mut usize,
    unsupported_recurrence_events: &mut usize,
) {
    match parse_event_lines(lines, config) {
        Ok(Some(event)) => {
            if event.unsupported_recurrence {
                *unsupported_recurrence_events += 1;
            }
            events.push(event);
        }
        Ok(None) => {}
        Err(_) => *invalid_events += 1,
    }
}

fn validate_complete_calendar(state: CalendarState) -> Result<(), TuliproxError> {
    match state {
        CalendarState::Before => Err(invalid_calendar_structure("missing BEGIN:VCALENDAR")),
        CalendarState::Inside => Err(invalid_calendar_structure("missing END:VCALENDAR")),
        CalendarState::After => Ok(()),
    }
}

fn log_parse_summary(
    invalid_events: usize,
    unsupported_recurrence_events: usize,
    examined_events: usize,
    event_limit_reached: bool,
    max_events: usize,
) {
    if invalid_events != 0 {
        warn!("Skipped {invalid_events} invalid ICS VEVENT block(s) while examining {examined_events} block(s)");
    }
    if event_limit_reached {
        warn!("ICS VEVENT processing limit {max_events} reached; additional event blocks were not examined");
    }
    if unsupported_recurrence_events != 0 {
        warn!(
            "Detected {unsupported_recurrence_events} ICS VEVENT block(s) with unsupported recurrence properties \
             (RRULE/RDATE/EXDATE); importing only their base DTSTART/DTEND occurrence"
        );
    }
}

fn invalid_calendar_structure(reason: &str) -> TuliproxError {
    TuliproxError::Parse(format!("Invalid ICS VCALENDAR structure: {reason}"))
}

fn is_component_boundary(line: &str, boundary: &str, component: &str) -> bool {
    let Some((left, value)) = line.split_once(':') else {
        return false;
    };
    let name = left.split(';').next().unwrap_or_default().trim();
    name.eq_ignore_ascii_case(boundary) && value.eq_ignore_ascii_case(component)
}

fn unfold_ics_lines(input: &str) -> Result<Vec<String>, TuliproxError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut result: Vec<String> = Vec::new();

    for line in normalized.lines() {
        if line.len() > MAX_ICS_LINE_LENGTH {
            return Err(TuliproxError::Parse(format!(
                "ICS line exceeds max line length of {MAX_ICS_LINE_LENGTH} bytes"
            )));
        }

        if line.starts_with(' ') || line.starts_with('\t') {
            let Some(last) = result.last_mut() else {
                return Err(TuliproxError::Parse("ICS folded line has no preceding content line".to_string()));
            };
            last.push_str(&line[1..]);
            if last.len() > MAX_ICS_LINE_LENGTH {
                return Err(TuliproxError::Parse(format!(
                    "ICS unfolded line exceeds max line length of {MAX_ICS_LINE_LENGTH} bytes"
                )));
            }
        } else {
            result.push(line.to_string());
        }
    }

    Ok(result)
}

fn parse_event_lines(lines: &[String], config: &IcsEpgSourceConfig) -> Result<Option<IcsEvent>, TuliproxError> {
    let mut event = IcsEvent::default();
    let mut duration_seconds: Option<i64> = None;
    let mut all_day = false;
    let mut property_count = 0usize;
    let mut start_display_timezone: Option<Tz> = None;

    for line in lines {
        let Some(property) = parse_property(line) else {
            continue;
        };

        property_count += 1;
        if property_count > MAX_ICS_PROPERTIES_PER_EVENT {
            return Err(TuliproxError::Parse(format!(
                "VEVENT has more than {MAX_ICS_PROPERTIES_PER_EVENT} properties"
            )));
        }

        match property.name.as_str() {
            "UID" => event.uid = Some(unescape_ics_text(&property.value)),
            "SUMMARY" => {
                event.summary =
                    Some(truncate_to_byte_limit(unescape_ics_text(&property.value), MAX_ICS_SUMMARY_LENGTH));
            }
            "DESCRIPTION" => {
                event.description =
                    Some(truncate_to_byte_limit(unescape_ics_text(&property.value), MAX_ICS_DESCRIPTION_LENGTH));
            }
            "LOCATION" => event.location = Some(unescape_ics_text(&property.value)),
            "CATEGORIES" => event.categories.extend(parse_ics_text_list(&property.value)),
            "STATUS" if property.value.eq_ignore_ascii_case("CANCELLED") => event.cancelled = true,
            "RRULE" | "RDATE" | "EXDATE" => event.unsupported_recurrence = true,
            "DTSTART" => {
                let parsed = parse_ics_datetime(&property.params, &property.value, &config.timezone)?;
                all_day |= parsed.all_day;
                start_display_timezone = Some(parsed.display_timezone);
                event.start = Some(parsed.timestamp);
                event.start_display = Some(parsed.display);
            }
            "DTEND" => {
                let parsed = parse_ics_datetime(&property.params, &property.value, &config.timezone)?;
                all_day |= parsed.all_day;
                event.stop = Some(parsed.timestamp);
                event.stop_display = Some(parsed.display);
            }
            "DURATION" => duration_seconds = parse_ics_duration(&property.value),
            _ => {}
        }
    }

    if all_day {
        return Ok(None);
    }

    if event.stop.is_none() {
        if let (Some(start), Some(duration)) = (event.start, duration_seconds) {
            let stop = start.checked_add(duration).ok_or_else(|| {
                TuliproxError::Parse("VEVENT start plus DURATION exceeds timestamp range".to_string())
            })?;
            let stop_display = start_display_timezone
                .and_then(|timezone| display_from_timestamp_in_timezone(stop, timezone))
                .ok_or_else(|| {
                    TuliproxError::Parse(
                        "VEVENT start plus DURATION is outside the supported datetime range".to_string(),
                    )
                })?;
            event.stop = Some(stop);
            // DURATION has no own timezone. Keep {end} in the same display timezone as DTSTART.
            event.stop_display = Some(stop_display);
        }
    }

    if event.start.is_none() || event.stop.is_none() {
        return Ok(None);
    }

    Ok(Some(event))
}

fn truncate_to_byte_limit(mut value: String, max_len: usize) -> String {
    if value.len() <= max_len {
        return value;
    }

    // Parser limits are byte-oriented because input size controls cost; truncate on a UTF-8 boundary.
    let mut end = max_len;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn parse_property(line: &str) -> Option<IcsProperty> {
    let (left, value) = line.split_once(':')?;
    let mut parts = left.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let mut params = IcsPropertyParams::new();

    for part in parts {
        if let Some((key, value)) = part.split_once('=') {
            params.insert(key.trim().to_ascii_uppercase(), value.trim_matches('"').to_string());
        }
    }

    Some(IcsProperty { name, params, value: value.to_string() })
}

fn unescape_ics_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();

    while let Some(character) = chars.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }

        match chars.next() {
            Some('n' | 'N') => result.push('\n'),
            Some(',') => result.push(','),
            Some(';') => result.push(';'),
            Some('\\') | None => result.push('\\'),
            Some(next) => {
                result.push('\\');
                result.push(next);
            }
        }
    }

    result
}

fn parse_ics_text_list(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut escaped = false;

    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ',' {
            push_ics_text_list_value(&mut values, &value[start..index]);
            start = index + character.len_utf8();
        }
    }
    push_ics_text_list_value(&mut values, &value[start..]);
    values
}

fn push_ics_text_list_value(values: &mut Vec<String>, raw: &str) {
    let value = unescape_ics_text(raw);
    let value = value.trim();
    if !value.is_empty() {
        values.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IcsEpgSourceConfig;
    use std::fmt::Write;

    fn config() -> IcsEpgSourceConfig { IcsEpgSourceConfig::default() }

    fn calendar(body: &str) -> String { format!("BEGIN:VCALENDAR\n{body}\nEND:VCALENDAR") }

    #[test]
    fn parses_utc_event_with_start_and_end() {
        let content = calendar(
            "BEGIN:VEVENT\nUID:1\nSUMMARY:Practice 1\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Practice 1"));
        assert_eq!(events[0].start, Some(1_772_800_200));
        assert_eq!(events[0].stop, Some(1_772_803_800));
    }

    #[test]
    fn categories_split_only_on_unescaped_commas() {
        let content = calendar(
            "BEGIN:VEVENT\nCATEGORIES: Sports ,Team\\, Regional,,News\nCATEGORIES:Path\\\\,Extra\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events[0].categories, vec!["Sports", "Team, Regional", "News", "Path\\", "Extra"]);
    }

    #[test]
    fn duration_supplies_missing_end() {
        let content =
            calendar("BEGIN:VEVENT\nSUMMARY:Qualifying\nDTSTART:20260306T123000Z\nDURATION:PT90M\nEND:VEVENT");
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stop, Some(1_772_805_600));
    }

    #[test]
    fn duration_uses_start_timezone_for_end_display() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Qualifying\nDTSTART;TZID=Europe/Berlin:20260306T123000\nDURATION:PT90M\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start_display.as_deref(), Some("2026-03-06T12:30:00+01:00"));
        assert_eq!(events[0].stop_display.as_deref(), Some("2026-03-06T14:00:00+01:00"));
    }

    #[test]
    fn folded_lines_are_unfolded_and_text_is_unescaped() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Long\n DESCRIPTION\nDESCRIPTION:Line 1\\nLine 2\\, ok\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events[0].summary.as_deref(), Some("LongDESCRIPTION"));
        assert_eq!(events[0].description.as_deref(), Some("Line 1\nLine 2, ok"));
    }

    #[test]
    fn text_unescaping_decodes_only_known_sequences() {
        assert_eq!(unescape_ics_text(r"line\nnext\Ncomma\,semi\;slash\\"), "line\nnext\ncomma,semi;slash\\");
        assert_eq!(unescape_ics_text(r"literal\\n literal\\, literal\\;"), r"literal\n literal\, literal\;");
        assert_eq!(unescape_ics_text(r"unknown\x trailing\"), r"unknown\x trailing\");
    }

    #[test]
    fn event_without_start_or_end_is_skipped() {
        let missing_start = calendar("BEGIN:VEVENT\nSUMMARY:No start\nDTEND:20260306T133000Z\nEND:VEVENT");
        let missing_end = calendar("BEGIN:VEVENT\nSUMMARY:No end\nDTSTART:20260306T123000Z\nEND:VEVENT");
        assert!(parse_ics_events(&missing_start, &config()).expect("events").is_empty());
        assert!(parse_ics_events(&missing_end, &config()).expect("events").is_empty());
    }

    #[test]
    fn malformed_event_is_skipped_while_following_event_is_imported() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Bad\nDTSTART:bad\nDTEND:20260306T133000Z\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Good\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Good"));
    }

    #[test]
    fn mixed_case_vevent_boundaries_are_recognized() {
        let content = calendar(
            "begin:VEVENT\nSUMMARY:Lower\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nend:VEVENT\nBegin:Vevent\nSUMMARY:Mixed\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(
            events.iter().filter_map(|event| event.summary.as_deref()).collect::<Vec<_>>(),
            vec!["Lower", "Mixed"]
        );
    }

    #[test]
    fn unknown_event_tzid_skips_only_that_event() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Bad TZ\nDTSTART;TZID=Mars/Olympus:20260306T123000\nDTEND;TZID=Mars/Olympus:20260306T133000\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Good\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Good"));
    }

    #[test]
    fn max_events_is_enforced_without_rrule_expansion() {
        let cfg = IcsEpgSourceConfig { max_events: 1, ..IcsEpgSourceConfig::default() };
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:One\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nRRULE:FREQ=DAILY;COUNT=10\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Two\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &cfg).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("One"));
    }

    #[test]
    fn recurring_event_properties_are_detected_without_expansion() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Recurring\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nRRULE:FREQ=DAILY;COUNT=10\nEXDATE:20260307T123000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert!(events[0].unsupported_recurrence);
    }

    #[test]
    fn invalid_events_consume_the_same_event_budget_as_valid_events() {
        let cfg = IcsEpgSourceConfig { max_events: 2, ..IcsEpgSourceConfig::default() };
        let only_invalid_before_valid = calendar(
            "BEGIN:VEVENT\nDTSTART:bad-1\nDTEND:bad-1\nEND:VEVENT\nBEGIN:VEVENT\nDTSTART:bad-2\nDTEND:bad-2\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:After limit\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        assert!(parse_ics_events(&only_invalid_before_valid, &cfg).expect("events").is_empty());

        let mixed = calendar(
            "BEGIN:VEVENT\nDTSTART:bad\nDTEND:bad\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Within budget\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:After limit\nDTSTART:20260308T123000Z\nDTEND:20260308T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&mixed, &cfg).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Within budget"));
    }

    #[test]
    fn nested_vevent_boundaries_consume_the_event_budget() {
        let cfg = IcsEpgSourceConfig { max_events: 2, ..IcsEpgSourceConfig::default() };
        let content = calendar(
            "BEGIN:VEVENT\nBEGIN:VEVENT\nDTSTART:bad\nEND:VEVENT\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:After nested limit\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        assert!(parse_ics_events(&content, &cfg).expect("events").is_empty());
    }

    #[test]
    fn all_day_events_are_skipped() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:All day\nDTSTART;VALUE=DATE:20260306\nDTEND;VALUE=DATE:20260307\nEND:VEVENT",
        );
        assert!(parse_ics_events(&content, &config()).expect("events").is_empty());
    }

    #[test]
    fn event_with_too_many_properties_is_skipped() {
        let mut body = String::from("BEGIN:VEVENT\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\n");
        for idx in 0..=MAX_ICS_PROPERTIES_PER_EVENT {
            writeln!(body, "X-PROP-{idx}:value").expect("write property fixture");
        }
        body.push_str(
            "END:VEVENT\nBEGIN:VEVENT\nSUMMARY:Good\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let content = calendar(&body);
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Good"));
    }

    #[test]
    fn too_long_lines_fail_source_parse() {
        let content = calendar(&format!("BEGIN:VEVENT\nSUMMARY:{}\nEND:VEVENT", "x".repeat(MAX_ICS_LINE_LENGTH + 1)));
        let err = parse_ics_events(&content, &config()).unwrap_err();
        assert!(err.to_string().contains("max line length"));
    }

    #[test]
    fn overlong_summary_and_description_are_truncated() {
        let content = calendar(&format!(
            "BEGIN:VEVENT\nSUMMARY:{}\nDESCRIPTION:{}\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT",
            "s".repeat(MAX_ICS_SUMMARY_LENGTH + 10),
            "d".repeat(MAX_ICS_DESCRIPTION_LENGTH + 10),
        ));
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref().map(str::len), Some(MAX_ICS_SUMMARY_LENGTH));
        assert_eq!(events[0].description.as_deref().map(str::len), Some(MAX_ICS_DESCRIPTION_LENGTH));
    }

    #[test]
    fn valid_empty_and_case_insensitive_bom_calendars_are_allowed() {
        assert!(parse_ics_events("BEGIN:VCALENDAR\nEND:VCALENDAR", &config()).expect("empty calendar").is_empty());
        assert!(parse_ics_events("\u{feff}begin:vcalendar\nend:vcalendar", &config())
            .expect("case-insensitive calendar with BOM")
            .is_empty());
    }

    #[test]
    fn non_calendar_text_and_html_are_rejected() {
        for content in ["garbage", "<html><body>upstream error</body></html>"] {
            let err = parse_ics_events(content, &config()).unwrap_err();
            assert!(err.to_string().contains("VCALENDAR structure"));
        }
    }

    #[test]
    fn incomplete_reversed_and_nested_calendar_envelopes_are_rejected() {
        for content in [
            "BEGIN:VCALENDAR\nVERSION:2.0",
            "END:VCALENDAR\nBEGIN:VCALENDAR",
            "BEGIN:VCALENDAR\nBEGIN:VCALENDAR\nEND:VCALENDAR\nEND:VCALENDAR",
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART:20260306T123000Z\nEND:VCALENDAR",
        ] {
            let err = parse_ics_events(content, &config()).unwrap_err();
            assert!(err.to_string().contains("VCALENDAR structure"));
        }
    }

    #[test]
    fn duration_timestamp_overflow_skips_only_that_event() {
        let content = calendar(
            "BEGIN:VEVENT\nSUMMARY:Overflow\nDTSTART:20260306T123000Z\nDURATION:PT9223372036854775807S\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Good\nDTSTART:20260307T123000Z\nDTEND:20260307T133000Z\nEND:VEVENT",
        );
        let events = parse_ics_events(&content, &config()).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("Good"));
    }
}
