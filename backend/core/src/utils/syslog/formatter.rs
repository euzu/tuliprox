use chrono::{DateTime, Local, SecondsFormat};
use shared::model::{LogLevel, SyslogFacility, SyslogFormat};
use std::{borrow::Cow, fmt::Write as _};

pub(crate) struct SyslogMeta<'a> {
    pub timestamp: DateTime<Local>,
    pub level: log::Level,
    pub target: &'a str,
    pub message: &'a str,
}

pub(crate) struct SyslogHeader {
    pub format: SyslogFormat,
    pub facility: SyslogFacility,
    pub app_name: String,
    pub hostname: String,
    pub pid: u32,
    pub max_len: usize,
}

#[derive(serde::Serialize)]
struct SyslogJsonPayload<'a> {
    #[serde(rename = "@timestamp")]
    timestamp: &'a str,
    level: LogLevel,
    facility: SyslogFacility,
    app_name: &'a str,
    hostname: &'a str,
    pid: u32,
    target: &'a str,
    message: &'a str,
    truncated: bool,
}

/// True for the bytes a header field may carry: PRINTUSASCII without the two JSON metacharacters.
/// Excluding `"` and `\\` is what bounds the JSON object — no field can grow when it is serialized.
fn is_field_byte(b: u8) -> bool { (0x21..=0x7E).contains(&b) && b != b'"' && b != b'\\' }

/// Appends at most `max_len` sanitized bytes of `src`. Writes into the caller's buffer, so a record does not
/// allocate one `String` per header field.
fn push_sanitized(out: &mut String, src: &str, max_len: usize) {
    let start = out.len();
    for b in src.bytes() {
        if out.len() - start >= max_len {
            break;
        }
        out.push(if is_field_byte(b) { b as char } else { '_' });
    }
}

/// Appends the sanitized field, or `-` (NILVALUE) when it sanitizes to nothing.
fn push_field_or_nil(out: &mut String, src: &str, max_len: usize) {
    let start = out.len();
    push_sanitized(out, src, max_len);
    if out.len() == start {
        out.push('-');
    }
}

/// Borrows `src` when it needs no sanitizing, so the common record costs no extra allocation.
#[must_use]
pub(crate) fn sanitize_if_needed(src: &str, max_len: usize) -> Cow<'_, str> {
    if src.len() <= max_len && src.bytes().all(is_field_byte) {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len().min(max_len));
    push_sanitized(&mut out, src, max_len);
    Cow::Owned(out)
}

#[must_use]
pub(crate) fn sanitize_header_field(s: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_len) + 1);
    push_field_or_nil(&mut out, s, max_len);
    out
}

#[must_use]
pub(crate) fn sanitize_message(message: &str) -> Cow<'_, str> {
    if !message.as_bytes().iter().any(|&b| b <= 0x1F || b == 0x7F) {
        return Cow::Borrowed(message);
    }
    let mut bytes = message.as_bytes().to_vec();
    for b in &mut bytes {
        if *b <= 0x1F || *b == 0x7F {
            *b = b' ';
        }
    }
    match String::from_utf8(bytes) {
        Ok(s) => Cow::Owned(s),
        Err(_) => Cow::Borrowed(message),
    }
}

fn calculate_pri(facility: SyslogFacility, level: log::Level) -> u8 {
    let severity = match level {
        log::Level::Error => 3,
        log::Level::Warn => 4,
        log::Level::Info => 6,
        log::Level::Debug | log::Level::Trace => 7,
    };
    facility.code() * 8 + severity
}

fn extract_msgid(target: &str) -> &str { target.rsplit("::").next().unwrap_or("") }

#[must_use]
pub(crate) fn format(meta: &SyslogMeta<'_>, header: &SyslogHeader) -> (String, bool) {
    match header.format {
        SyslogFormat::Rfc5424 => format_rfc5424(meta, header),
        SyslogFormat::Rfc3164 => format_rfc3164(meta, header),
        SyslogFormat::Json => format_json(meta, header),
    }
}

fn format_rfc5424(meta: &SyslogMeta<'_>, header: &SyslogHeader) -> (String, bool) {
    let pri = calculate_pri(header.facility, meta.level);
    let mut out = String::with_capacity(header.max_len);
    let _ = write!(&mut out, "<{pri}>1 ");
    let _ = write!(&mut out, "{}", meta.timestamp.to_rfc3339_opts(SecondsFormat::Micros, false));
    out.push(' ');
    push_field_or_nil(&mut out, &header.hostname, 255);
    out.push(' ');
    push_field_or_nil(&mut out, &header.app_name, 48);
    let _ = write!(&mut out, " {} ", header.pid);
    push_field_or_nil(&mut out, extract_msgid(meta.target), 32);
    out.push_str(" - ");
    append_message(out, meta.message, header.max_len)
}

fn format_rfc3164(meta: &SyslogMeta<'_>, header: &SyslogHeader) -> (String, bool) {
    let pri = calculate_pri(header.facility, meta.level);
    let mut out = String::with_capacity(header.max_len);
    let _ = write!(&mut out, "<{pri}>");
    let _ = write!(&mut out, "{}", meta.timestamp.format("%b %e %H:%M:%S"));
    out.push(' ');
    push_field_or_nil(&mut out, &header.hostname, 255);
    out.push(' ');
    // RFC 3164 §4.1.3 caps TAG at 32 characters; parsers that enforce it would otherwise read the rest as MSG.
    push_field_or_nil(&mut out, &header.app_name, 32);
    let _ = write!(&mut out, "[{}]: ", header.pid);
    append_message(out, meta.message, header.max_len)
}

/// Second phase of the formatter contract: the prefix is complete, so only the message is cut, at a char boundary.
fn append_message(mut out: String, message: &str, max_len: usize) -> (String, bool) {
    let budget = max_len.saturating_sub(out.len());
    let sanitized = sanitize_message(message);
    let truncated = sanitized.len() > budget;
    if truncated {
        let cut = sanitized.floor_char_boundary(budget);
        out.push_str(&sanitized[..cut]);
    } else {
        out.push_str(&sanitized);
    }
    (out, truncated)
}

/// Longest char-boundary prefix of `message` whose JSON-escaped form fits in `budget` bytes. `sanitize_message`
/// already replaced every control byte, so only `"` and `\\` grow when serialized, by one byte each.
fn json_escaped_prefix_len(message: &str, budget: usize) -> usize {
    let mut escaped = 0;
    for (idx, ch) in message.char_indices() {
        escaped += ch.len_utf8() + usize::from(matches!(ch, '"' | '\\'));
        if escaped > budget {
            return idx;
        }
    }
    message.len()
}

fn format_json(meta: &SyslogMeta<'_>, header: &SyslogHeader) -> (String, bool) {
    let ts = meta.timestamp.to_rfc3339_opts(SecondsFormat::Micros, false);
    let level = LogLevel::from(meta.level);
    let host = sanitize_if_needed(&header.hostname, 255);
    let app = sanitize_if_needed(&header.app_name, 48);
    let target = sanitize_if_needed(meta.target, 255);
    let sanitized_msg = sanitize_message(meta.message);

    let serialize = |msg: &str, trunc: bool| -> String {
        let payload = SyslogJsonPayload {
            timestamp: &ts,
            level,
            facility: header.facility,
            app_name: &app,
            hostname: &host,
            pid: header.pid,
            target: &target,
            message: msg,
            truncated: trunc,
        };
        serde_json::to_string(&payload).unwrap_or_default()
    };

    // NDJSON: every record ends with the newline delimiter, and the delimiter counts against `max_len`.
    let max_len = header.max_len.saturating_sub(1);
    let mut full_serialized = serialize(&sanitized_msg, false);
    if full_serialized.len() <= max_len {
        full_serialized.push('\n');
        return (full_serialized, false);
    }

    let empty_serialized = serialize("", true);
    let budget = max_len.saturating_sub(empty_serialized.len());
    let cut_len = json_escaped_prefix_len(&sanitized_msg, budget);
    let mut serialized = serialize(&sanitized_msg[..cut_len], true);
    serialized.push('\n');

    debug_assert!(serialized.len() <= header.max_len);
    (serialized, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike as _;

    #[test]
    fn test_1_rfc5424_exact_bytes_and_pri() {
        let fixed_dt = DateTime::parse_from_rfc3339("2026-10-27T17:45:00.123456+01:00").unwrap().with_timezone(&Local);
        let meta = SyslogMeta {
            timestamp: fixed_dt,
            level: log::Level::Info,
            target: "tuliprox_app::api::main_api",
            message: "Client stream allocated",
        };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 12345,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(!truncated);
        let ts = fixed_dt.to_rfc3339_opts(SecondsFormat::Micros, false);
        let expected = format!("<134>1 {ts} node1 tuliprox 12345 main_api - Client stream allocated");
        assert_eq!(out, expected);

        // PRI checks: local0+info = 134, daemon+error = 27
        assert_eq!(calculate_pri(SyslogFacility::Local0, log::Level::Info), 134);
        assert_eq!(calculate_pri(SyslogFacility::Daemon, log::Level::Error), 27);
    }

    #[test]
    fn test_2_msgid_derivation_and_header_limits() {
        let now = Local::now();
        // Derivation a::b::c -> c
        let meta = SyslogMeta { timestamp: now, level: log::Level::Info, target: "a::b::c", message: "test" };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "a".repeat(60),
            hostname: "h".repeat(300),
            pid: 1,
            max_len: 1024,
        };
        let (out, _) = format(&meta, &header);
        assert!(out.contains(" c - "));
        assert!(out.contains(&"a".repeat(48)));
        assert!(!out.contains(&"a".repeat(49)));
        assert!(out.contains(&"h".repeat(255)));
        assert!(!out.contains(&"h".repeat(256)));

        // '-' for empty hostname and empty target
        let empty_header = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "app".to_string(),
            hostname: String::new(),
            pid: 1,
            max_len: 1024,
        };
        let empty_target_meta = SyslogMeta { timestamp: now, level: log::Level::Info, target: "", message: "test" };
        let (out_empty, _) = format(&empty_target_meta, &empty_header);
        assert!(out_empty.contains(" - app 1 - - test"));

        // 40-byte single segment truncated to 32
        let long_segment_meta = SyslogMeta {
            timestamp: now,
            level: log::Level::Info,
            target: "prefix::1234567890123456789012345678901234567890",
            message: "test",
        };
        let (out_long_seg, _) = format(&long_segment_meta, &empty_header);
        assert!(out_long_seg.contains(" 12345678901234567890123456789012 - test"));
    }

    #[test]
    fn test_3_rfc3164_space_padded_day_and_exact_bytes() {
        let now = Local::now();
        let date_7 = now.with_day(7).unwrap_or(now);
        let meta = SyslogMeta {
            timestamp: date_7,
            level: log::Level::Info,
            target: "main_api",
            message: "Client stream allocated",
        };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc3164,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 12345,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(!truncated);
        let ts = date_7.format("%b %e %H:%M:%S").to_string();
        if date_7.day() < 10 {
            assert!(ts.contains("  "));
        }
        let expected = format!("<134>{ts} node1 tuliprox[12345]: Client stream allocated");
        assert_eq!(out, expected);
    }

    #[test]
    fn test_4_json_parses_single_line() {
        let meta = SyslogMeta {
            timestamp: Local::now(),
            level: log::Level::Info,
            target: "tuliprox_app::api::main_api",
            message: "Client stream allocated\nwith newline",
        };
        let header = SyslogHeader {
            format: SyslogFormat::Json,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 12345,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(!truncated);
        let line = out.strip_suffix('\n').expect("NDJSON record must end with a newline");
        assert!(!line.contains('\n'));
        let val: serde_json::Value = serde_json::from_str(line).expect("must parse as json");
        assert_eq!(val["level"], "info");
        assert_eq!(val["facility"], "local0");
        assert_eq!(val["app_name"], "tuliprox");
        assert_eq!(val["hostname"], "node1");
        assert_eq!(val["pid"], 12345);
        assert_eq!(val["target"], "tuliprox_app::api::main_api");
        assert_eq!(val["message"], "Client stream allocated with newline");
        assert_eq!(val["truncated"], false);
    }

    #[test]
    fn test_5_sanitizing_control_chars_and_multibyte_utf8() {
        let meta = SyslogMeta {
            timestamp: Local::now(),
            level: log::Level::Warn,
            target: "test",
            message: "hello\r\n\0world",
        };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 100,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(!truncated);
        assert!(out.ends_with("hello   world"));

        let crab_msg = "🦀".repeat(20);
        let meta_crab =
            SyslogMeta { timestamp: Local::now(), level: log::Level::Warn, target: "test", message: &crab_msg };
        let (out_ref, _) = format(
            &SyslogMeta { timestamp: Local::now(), level: log::Level::Warn, target: "test", message: "" },
            &header,
        );
        let header_tight = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 100,
            max_len: out_ref.len() + 10,
        };
        let (out_crab, truncated_crab) = format(&meta_crab, &header_tight);
        assert!(truncated_crab);
        assert!(out_crab.ends_with("🦀🦀"));
        assert_eq!(out_crab.len(), out_ref.len() + 8);
    }

    #[test]
    fn test_6_total_length_invariant() {
        for format_choice in [SyslogFormat::Rfc5424, SyslogFormat::Rfc3164, SyslogFormat::Json] {
            let max_len = if format_choice == SyslogFormat::Json { 1024 } else { 480 };
            let header = SyslogHeader {
                format: format_choice,
                facility: SyslogFacility::Local0,
                app_name: "tuliprox".to_string(),
                hostname: "node1".to_string(),
                pid: 1234,
                max_len,
            };
            let empty_meta =
                SyslogMeta { timestamp: Local::now(), level: log::Level::Info, target: "target", message: "" };
            let (ref_out, _) = format(&empty_meta, &header);

            for len in [0, 1, max_len - 1, max_len, max_len + 1, 10 * max_len] {
                let msg = "x".repeat(len);
                let meta = SyslogMeta {
                    timestamp: empty_meta.timestamp,
                    level: log::Level::Info,
                    target: "target",
                    message: &msg,
                };
                let (out, truncated) = format(&meta, &header);
                assert!(
                    out.len() <= max_len,
                    "Format {format_choice:?} with msg len {len} produced {} > {max_len}",
                    out.len()
                );
                if format_choice != SyslogFormat::Json {
                    assert!(out.starts_with(&ref_out), "Prefix mismatch for {format_choice:?} with msg len {len}");
                }
                if len > max_len {
                    assert!(truncated);
                }
            }
        }
    }

    #[test]
    fn test_7_json_stays_valid_after_truncation() {
        let raw_msg = "Hello \"world\" \\ test \n\r\t üñîçødé 🦀 special characters ".repeat(30);
        let meta = SyslogMeta {
            timestamp: Local::now(),
            level: log::Level::Info,
            target: "tuliprox_app::api::main_api",
            message: &raw_msg,
        };
        let header = SyslogHeader {
            format: SyslogFormat::Json,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: "node1".to_string(),
            pid: 12345,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(truncated);
        assert!(out.len() <= 1024);
        let val: serde_json::Value = serde_json::from_str(&out).expect("must parse after truncation");
        let msg = val["message"].as_str().expect("message field must be string");
        let sanitized = sanitize_message(&raw_msg);
        assert!(sanitized.starts_with(msg));
        assert_eq!(val["truncated"], true);

        // One cut fills the budget: escaped quotes cost two bytes each, so the gap stays below one escaped char.
        let quotes = "\"".repeat(2000);
        let meta_quotes = SyslogMeta { message: &quotes, ..meta };
        let (out_quotes, truncated_quotes) = format(&meta_quotes, &header);
        assert!(truncated_quotes);
        assert!(out_quotes.ends_with('\n'), "truncated NDJSON record must end with a newline");
        assert!(out_quotes.len() <= 1024 && out_quotes.len() >= 1022, "len {}", out_quotes.len());
        serde_json::from_str::<serde_json::Value>(&out_quotes).expect("must parse after truncation");
    }

    #[test]
    fn test_7b_rfc3164_tag_capped_at_32_chars() {
        let meta = SyslogMeta { timestamp: Local::now(), level: log::Level::Info, target: "t", message: "msg" };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc3164,
            facility: SyslogFacility::Local0,
            app_name: "a".repeat(48),
            hostname: "node1".to_string(),
            pid: 7,
            max_len: 1024,
        };
        let (out, _) = format(&meta, &header);
        assert!(out.contains(&format!(" {}[7]: msg", "a".repeat(32))), "{out}");
    }

    #[test]
    fn test_8_worst_case_header_arithmetic() {
        let fixed_dt = DateTime::parse_from_rfc3339("2026-10-27T17:45:00.123456+01:00").unwrap().with_timezone(&Local);
        let meta = SyslogMeta { timestamp: fixed_dt, level: log::Level::Trace, target: &"m".repeat(32), message: "" };
        let header_5424 = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local7,
            app_name: "a".repeat(48),
            hostname: "h".repeat(255),
            pid: 4_294_967_295,
            max_len: 480,
        };
        let (out_5424, _) = format(&meta, &header_5424);
        assert!(out_5424.len() <= 391, "RFC 5424 prefix {} > 391", out_5424.len());
        assert!(480 - out_5424.len() >= 89, "RFC 5424 message budget {} < 89", 480 - out_5424.len());

        let header_3164 = SyslogHeader {
            format: SyslogFormat::Rfc3164,
            facility: SyslogFacility::Local7,
            app_name: "a".repeat(48),
            hostname: "h".repeat(255),
            pid: 4_294_967_295,
            max_len: 480,
        };
        let (out_3164, _) = format(&meta, &header_3164);
        assert!(out_3164.len() <= 339, "RFC 3164 prefix {} > 339", out_3164.len());
        assert!(480 - out_3164.len() >= 89, "RFC 3164 message budget {} < 89", 480 - out_3164.len());
    }

    #[test]
    fn test_9_json_envelope_budget() {
        let fixed_dt = DateTime::parse_from_rfc3339("2026-10-27T17:45:00.123456+01:00").unwrap().with_timezone(&Local);
        let meta = SyslogMeta { timestamp: fixed_dt, level: log::Level::Trace, target: &"t".repeat(255), message: "" };
        let mut header = SyslogHeader {
            format: SyslogFormat::Json,
            // `authpriv` is the longest facility name, so this stays the worst-case envelope.
            facility: SyslogFacility::Authpriv,
            app_name: "a".repeat(48),
            hostname: "h".repeat(255),
            pid: 1_234_567_890,
            max_len: 1024,
        };
        let (out, truncated) = format(&meta, &header);
        assert!(!truncated);
        assert_eq!(out.len(), 734, "worst-case envelope plus the NDJSON newline");
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
        assert!(1024 - out.len() >= 256);

        let meta_300 =
            SyslogMeta { timestamp: fixed_dt, level: log::Level::Trace, target: &"t".repeat(300), message: "" };
        let (out_300, truncated_300) = format(&meta_300, &header);
        assert!(!truncated_300);
        assert_eq!(out_300.len(), 734);

        header.hostname = "\"".repeat(255);
        let (out_quotes, truncated_quotes) = format(&meta, &header);
        assert!(!truncated_quotes);
        assert_eq!(out_quotes.len(), 734);
    }
}
