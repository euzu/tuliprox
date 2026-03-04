use crate::api::model::AppState;
use crate::model::{ConfigTarget, ProxyUserCredentials};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use shared::model::PlaylistItemType;
use std::sync::Arc;

/// Parses user-defined EPG timeshift configuration.
/// Supports either a numeric offset (e.g. "+2:30", "-1:15")
/// or a timezone name (e.g. "`Europe/Berlin`", "`UTC`", "`America/New_York`").
pub fn parse_timeshift(time_shift: Option<&str>) -> EpgTimeShift {
    if let Some(offset) = time_shift {
        if offset.is_empty() {
            return EpgTimeShift::None;
        }

        // Try to parse as timezone name first
        if let Ok(tz) = offset.parse::<Tz>() {
            return EpgTimeShift::TimeZone(tz);
        }

        // If not a timezone, try to parse as numeric offset
        let sign_factor = if offset.starts_with('-') { -1 } else { 1 };
        let offset = offset.trim_start_matches(&['-', '+'][..]);

        let parts: Vec<&str> = offset.split(':').collect();
        let hours: i32 = parts.first().and_then(|h| h.parse().ok()).unwrap_or(0);
        let minutes: i32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);

        let total_minutes = hours * 60 + minutes;
        if total_minutes > 0 {
            EpgTimeShift::Fixed(sign_factor * total_minutes)
        } else {
            EpgTimeShift::None
        }
    } else {
        EpgTimeShift::None
    }
}


#[derive(Debug, Clone)]
pub enum EpgTimeShift {
    None,
    Fixed(i32),
    TimeZone(Tz),
}

#[derive(Debug, Clone)]
pub struct EpgProcessingOptions {
    pub rewrite_urls: bool,
    pub time_shift: EpgTimeShift,
    pub encrypt_secret: [u8; 16],
}

pub fn get_epg_processing_options(app_state: &Arc<AppState>, user: &ProxyUserCredentials, target: &Arc<ConfigTarget>) -> EpgProcessingOptions {
    let rewrite_resources = app_state.app_config.is_reverse_proxy_resource_rewrite_enabled();
    let encrypt_secret = app_state.get_encrypt_secret();

    // If redirect is true → rewrite_urls = false → keep original
    // If redirect is false and rewrite_resources is true → rewrite_urls = true → rewriting allowed
    // If redirect is false and rewrite_resources is false → rewrite_urls = false → no rewriting
    let redirect = user.proxy.is_redirect(PlaylistItemType::Live) || target.is_force_redirect(PlaylistItemType::Live);
    let rewrite_urls = !redirect && rewrite_resources;

    let timeshift = parse_timeshift(user.epg_timeshift.as_deref());
    EpgProcessingOptions {
        rewrite_urls,
        time_shift: timeshift,
        encrypt_secret,
    }
}

pub fn apply_offset(ts_utc: i64, offset_minutes: i32) -> i64 {
    ts_utc + i64::from(offset_minutes) * 60
}

pub fn format_offset(offset_minutes: i32) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let abs = offset_minutes.abs();
    let hours = abs / 60;
    let mins = abs % 60;
    format!("{sign}{hours:02}{mins:02}")
}

pub fn parse_xmltv_time(t: &str) -> Option<i64> {
    DateTime::parse_from_str(t, "%Y%m%d%H%M%S %z")
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp())
}

pub fn format_xmltv_time_utc(ts: i64, time_shift: &EpgTimeShift) -> String {
    let dt = Utc.timestamp_opt(ts, 0).unwrap();
    match time_shift {
        EpgTimeShift::None => dt.format("%Y%m%d%H%M%S %z").to_string(),
        EpgTimeShift::Fixed(minutes) => {
            match chrono::FixedOffset::east_opt(minutes * 60) {
                Some(offset) => {
                    dt.with_timezone(&offset).format("%Y%m%d%H%M%S %z").to_string()
                }
                None => {
                    dt.format("%Y%m%d%H%M%S %z").to_string()
                }
            }
        }
        EpgTimeShift::TimeZone(tz) => {
            dt.with_timezone(tz).format("%Y%m%d%H%M%S %z").to_string()
        }
    }
}

pub fn apply_timeshift(date_str: &str, shift: &EpgTimeShift) -> String {
    if let EpgTimeShift::None = shift {
        return date_str.to_string();
    }

    // List of supported formats
    let formats = [
        "%Y-%m-%d:%H-%M", // 2026-02-08:11-30
        "%Y-%m-%d:%H:%M", // 2026-02-08:11:30
        "%Y-%m-%d %H:%M", // 2026-02-08 11:30
        "%Y-%m-%d-%H-%M", // 2026-02-08-11-30
    ];

    let mut parsed_dt = None;
    let mut matching_format = "";

    // 1. Try to parse date with supported formats
    for format in formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(date_str, format) {
            parsed_dt = Some(dt);
            matching_format = format;
            break;
        }
    }

    let (dt, is_ts) = if let Some(dt) = parsed_dt {
        (dt, false)
    } else if let Ok(ts) = date_str.parse::<i64>() {
        match DateTime::from_timestamp(ts, 0) {
            Some(d) => (d.naive_utc(), true),
            None => return date_str.to_string(),
        }
    } else {
        return date_str.to_string();
    };

    // 2. Calculate offset
    let shifted_dt = match shift {
        EpgTimeShift::Fixed(minutes) => {
            dt + Duration::minutes(i64::from(*minutes))
        }
        EpgTimeShift::TimeZone(tz) => {
            // We assume it is UTC and want to know the target TZ
            let utc_dt = Utc.from_utc_datetime(&dt);
            utc_dt.with_timezone(tz).naive_local()
        }
        EpgTimeShift::None => dt,
    };

    if is_ts {
        let utc_shifted = Utc.from_utc_datetime(&shifted_dt);
        utc_shifted.timestamp().to_string()
    } else {
        shifted_dt.format(matching_format).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeshift() {
        assert!(matches!(parse_timeshift(Some(&String::from("2"))), EpgTimeShift::Fixed(120)));
        assert!(matches!(parse_timeshift(Some("-1:30")), EpgTimeShift::Fixed(-90)));
        assert!(matches!(parse_timeshift(Some("+0:15")), EpgTimeShift::Fixed(15)));
        assert!(matches!(parse_timeshift(Some("1:45")), EpgTimeShift::Fixed(105)));
        assert!(matches!(parse_timeshift(Some(":45")), EpgTimeShift::Fixed(45)));
        assert!(matches!(parse_timeshift(Some("-:45")), EpgTimeShift::Fixed(-45)));
        assert!(matches!(parse_timeshift(Some("0:30")), EpgTimeShift::Fixed(30)));
        assert!(matches!(parse_timeshift(Some(":3")), EpgTimeShift::Fixed(3)));
        assert!(matches!(parse_timeshift(Some("2:")), EpgTimeShift::Fixed(120)));
        assert!(matches!(parse_timeshift(Some("+2:00")), EpgTimeShift::Fixed(120)));
        assert!(matches!(parse_timeshift(Some("-0:10")), EpgTimeShift::Fixed(-10)));
        assert!(matches!(parse_timeshift(Some("invalid")), EpgTimeShift::None));
        assert!(matches!(parse_timeshift(Some("+abc")), EpgTimeShift::None));
        assert!(matches!(parse_timeshift(Some("")), EpgTimeShift::None));
        assert!(matches!(parse_timeshift(None), EpgTimeShift::None));
    }

    #[test]
    fn test_parse_timezone() {
        // Check timezone parsing creates the correct variant
        let amterdam = parse_timeshift(Some("Europe/Amsterdam"));
        if let EpgTimeShift::TimeZone(tz) = amterdam {
            assert_eq!(tz.name(), "Europe/Amsterdam");
        } else {
            panic!("Expected TimeZone for Europe/Amsterdam");
        }

        let new_york = parse_timeshift(Some("America/New_York"));
        if let EpgTimeShift::TimeZone(tz) = new_york {
            assert_eq!(tz.name(), "America/New_York");
        } else {
            panic!("Expected TimeZone for America/New_York");
        }

        let tokyo = parse_timeshift(Some("Asia/Tokyo"));
        if let EpgTimeShift::TimeZone(tz) = tokyo {
            assert_eq!(tz.name(), "Asia/Tokyo");
        } else {
            panic!("Expected TimeZone for Asia/Tokyo");
        }

        let utc = parse_timeshift(Some("UTC"));
        if let EpgTimeShift::TimeZone(tz) = utc {
            assert_eq!(tz.name(), "UTC");
        } else {
            panic!("Expected TimeZone for UTC");
        }
    }

    #[test]
    fn test_apply_timeshift_formats() {
        let shift = EpgTimeShift::Fixed(60); // +1 hour

        // Original format: 2026-02-08:11-30
        let date_str = "2026-02-08:11-30";
        let shifted = apply_timeshift(date_str, &shift);
        assert_eq!(shifted, "2026-02-08:12-30");

        // Format with colons: 2026-02-08:11:30
        let date_str = "2026-02-08:11:30";
        let shifted = apply_timeshift(date_str, &shift);
        assert_eq!(shifted, "2026-02-08:12:30");

        // Format with space: 2026-02-08 11:30
        let date_str = "2026-02-08 11:30";
        let shifted = apply_timeshift(date_str, &shift);
        assert_eq!(shifted, "2026-02-08 12:30");

         // Format with dashes: 2026-02-08-11-30
        let date_str = "2026-02-08-11-30";
        let shifted = apply_timeshift(date_str, &shift);
        assert_eq!(shifted, "2026-02-08-12-30");

        // Timestamp
        let ts = 1_770_550_200; // 2026-02-08 11:30:00 UTC
        let date_str = ts.to_string();
        let shifted = apply_timeshift(&date_str, &shift);
        // +1 hour = +3600 seconds
        assert_eq!(shifted, (ts + 3600).to_string());
    }
}
