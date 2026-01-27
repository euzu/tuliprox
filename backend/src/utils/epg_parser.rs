use std::sync::Arc;
use chrono::{DateTime,  Offset, TimeZone, Utc};
use chrono_tz::Tz;
use crate::model::{ConfigTarget, ProxyUserCredentials};
use shared::model::PlaylistItemType;
use crate::api::model::AppState;


/// Parses user-defined EPG timeshift configuration.
/// Supports either a numeric offset (e.g. "+2:30", "-1:15")
/// or a timezone name (e.g. "`Europe/Berlin`", "`UTC`", "`America/New_York`").
///
/// Returns the total offset in minutes (i32).
fn parse_timeshift(time_shift: Option<&str>) -> Option<i32> {
    time_shift.and_then(|offset| {
        // Try to parse as timezone name first
        if let Ok(tz) = offset.parse::<Tz>() {
            // Determine the current UTC offset of that timezone (including DST)
            let now = Utc::now();
            let local_time = tz.from_utc_datetime(&now.naive_utc());
            let offset_minutes = local_time.offset().fix().local_minus_utc() / 60;
            return Some(offset_minutes);
        }

        // If not a timezone, try to parse as numeric offset
        let sign_factor = if offset.starts_with('-') { -1 } else { 1 };
        let offset = offset.trim_start_matches(&['-', '+'][..]);

        let parts: Vec<&str> = offset.split(':').collect();
        let hours: i32 = parts.first().and_then(|h| h.parse().ok()).unwrap_or(0);
        let minutes: i32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);

        let total_minutes = hours * 60 + minutes;
        (total_minutes > 0).then_some(sign_factor * total_minutes)
    })
}


#[derive(Debug, Clone)]
pub struct EpgProcessingOptions {
    pub rewrite_urls: bool,
    pub offset_minutes: i32,
    pub encrypt_secret: [u8; 16],
}

pub fn get_epg_processing_options(app_state: &Arc<AppState>, user: &ProxyUserCredentials, target: &Arc<ConfigTarget>) -> EpgProcessingOptions {
    let rewrite_resources = app_state.app_config.is_reverse_proxy_resource_rewrite_enabled();
    let encrypt_secret = app_state.app_config.get_reverse_proxy_rewrite_secret().unwrap_or_else(|| app_state.app_config.encrypt_secret);

    // If redirect is true → rewrite_urls = false → keep original
    // If redirect is false and rewrite_resources is true → rewrite_urls = true → rewriting allowed
    // If redirect is false and rewrite_resources is false → rewrite_urls = false → no rewriting
    let redirect = user.proxy.is_redirect(PlaylistItemType::Live) || target.is_force_redirect(PlaylistItemType::Live);
    let rewrite_urls = !redirect && rewrite_resources;

    // Use 0 for timeshift if None
    let timeshift = parse_timeshift(user.epg_timeshift.as_deref()).unwrap_or(0);
    EpgProcessingOptions {
        rewrite_urls,
        offset_minutes: timeshift,
        encrypt_secret,
    }
}
//
// pub trait EpgConsumer: Send {
//     fn handle_event(&mut self, event: &Event<'_>, decoder: quick_xml::Decoder) -> impl std::future::Future<Output = Result<(), TuliproxError>> + Send;
// }
//
// pub struct EpgProcessor<R: AsyncBufRead + Send + Unpin> {
//     reader: Reader<R>,
//     epg_processing_options: EpgProcessingOptions,
//     rewrite_base_url: String,
//     filter_channel_id: Option<std::sync::Arc<str>>,
//     limit: u32
// }
//
// impl<R: AsyncRead + Send + Unpin> EpgProcessor<BufReader<R>> {
//     pub fn new(
//         reader: R,
//         epg_processing_options: EpgProcessingOptions,
//         rewrite_base_url: String,
//         filter_channel_id: Option<std::sync::Arc<str>>,
//         limit: u32
//     ) -> Self {
//         Self {
//             reader: Reader::from_reader(BufReader::new(reader)),
//             epg_processing_options,
//             rewrite_base_url,
//             filter_channel_id,
//             limit
//         }
//     }
// }
//
// #[allow(clippy::too_many_lines)]
// impl<R: AsyncBufRead + Send + Unpin> EpgProcessor<R> {
//     pub async fn process<C: EpgConsumer>(&mut self, consumer: &mut C) -> Result<(), TuliproxError> {
//         let mut buf = Vec::with_capacity(4096);
//         let duration = Duration::minutes(i64::from(self.epg_processing_options.offset_minutes));
//         let mut skip_depth = None;
//
//         loop {
//             buf.clear();
//             let event = match self.reader.read_event_into_async(&mut buf).await {
//                 Ok(e) => e,
//                 Err(e) => {
//                     error!("Error reading epg XML event: {e}");
//                     return Err(info_err!("Error reading epg XML event: {}", e));
//                 }
//             };
//
//             if let Some(flt) = &self.filter_channel_id {
//                 match &event {
//                     Event::Start(e) => {
//                         if skip_depth.is_none() {
//                             let should_skip = match e.name().as_ref() {
//                                 b"channel" => {
//                                     e.attributes()
//                                         .filter_map(Result::ok)
//                                         .find(|a| a.key.as_ref() == b"id")
//                                         .and_then(|a| a.unescape_value().ok())
//                                         .is_some_and(|v| flt.as_ref() != v.as_ref())
//                                 }
//                                 b"programme" => {
//                                     e.attributes()
//                                         .filter_map(Result::ok)
//                                         .find(|a| a.key.as_ref() == b"channel")
//                                         .and_then(|a| a.unescape_value().ok())
//                                         .is_some_and(|v| flt.as_ref() != v.as_ref())
//                                 }
//                                 _ => false,
//                             };
//
//                             if should_skip {
//                                 skip_depth = Some(1);
//                                 continue;
//                             }
//                         } else {
//                             skip_depth = skip_depth.map(|d| d + 1);
//                             continue;
//                         }
//                     }
//                     Event::End(_) => {
//                         if let Some(depth) = skip_depth {
//                             if depth == 1 {
//                                 skip_depth = None;
//                             } else {
//                                 skip_depth = Some(depth - 1);
//                             }
//                             continue;
//                         }
//                     }
//                     Event::Empty(_) => {
//                         if skip_depth.is_some() {
//                             continue;
//                         }
//                     }
//                     _ => {}
//                 }
//
//                 if skip_depth.is_some() {
//                     continue;
//                 }
//             }
//
//             match event {
//                 Event::Start(ref e) if self.epg_processing_options.offset_minutes != 0 && e.name().as_ref() == b"programme" => {
//                     let mut elem = BytesStart::new(EPG_TAG_PROGRAMME);
//                     for attr in e.attributes() {
//                         match attr {
//                             Ok(attr) if attr.key.as_ref() == b"start" => {
//                                 if let Ok(start_value) = attr.decode_and_unescape_value(self.reader.decoder()) {
//                                     elem.push_attribute(("start", time_correct(&start_value, &duration).as_str()));
//                                 } else {
//                                     elem.push_attribute(attr);
//                                 }
//                             }
//                             Ok(attr) if attr.key.as_ref() == b"stop" => {
//                                 if let Ok(stop_value) = attr.decode_and_unescape_value(self.reader.decoder()) {
//                                     elem.push_attribute(("stop", time_correct(&stop_value, &duration).as_str()));
//                                 } else {
//                                     elem.push_attribute(attr);
//                                 }
//                             }
//                             Ok(attr) => {
//                                 elem.push_attribute(attr);
//                             }
//                             Err(e) => {
//                                 error!("Error parsing epg attribute: {e}");
//                             }
//                         }
//                     }
//                     consumer.handle_event(&Event::Start(elem), self.reader.decoder()).await?;
//                 }
//                 ref event @ (Event::Empty(ref e) | Event::Start(ref e)) if self.epg_processing_options.rewrite_urls && e.name().as_ref() == b"icon" => {
//                     let mut elem = BytesStart::new(EPG_TAG_ICON);
//                     for attr in e.attributes() {
//                         match attr {
//                             Ok(attr) if attr.key.as_ref() == b"src" => {
//                                 if let Some(icon) = get_attr_value_unescaped(&attr, self.reader.decoder()) {
//                                     if icon.is_empty() {
//                                         elem.push_attribute(attr);
//                                     } else {
//                                         let rewritten_url = if let Ok(encrypted) = obscure_text(&self.epg_processing_options.encrypt_secret, &icon) {
//                                             format!("{}{}", self.rewrite_base_url, encrypted)
//                                         } else {
//                                             icon
//                                         };
//                                         elem.push_attribute(("src", rewritten_url.as_str()));
//                                     }
//                                 } else {
//                                     elem.push_attribute(attr);
//                                 }
//                             }
//                             Ok(attr) => {
//                                 elem.push_attribute(attr);
//                             }
//                             Err(e) => {
//                                 error!("Error parsing epg attribute: {e}");
//                             }
//                         }
//                     }
//
//                     let out_event = match event {
//                         Event::Empty(_) => Event::Empty(elem),
//                         Event::Start(_) => Event::Start(elem),
//                         _ => unreachable!(),
//                     };
//                     consumer.handle_event(&out_event, self.reader.decoder()).await?;
//                 }
//                 Event::Decl(_) | Event::DocType(_) => {},
//                 Event::Eof => break,
//                 ref event => {
//                     consumer.handle_event(event, self.reader.decoder()).await?;
//                 }
//             }
//         }
//         Ok(())
//     }
// }
//
// /// # Panics
// /// unwrap for `FixedOffset` should not panic!
// pub fn time_correct(original: &str, shift: &Duration) -> String {
//     let (datetime_part, tz_part) = if let Some((dt, tz)) = original.trim().rsplit_once(' ') {
//         (dt, tz)
//     } else {
//         (original.trim(), "+0000")
//     };
//
//     let Ok(naive_dt) = NaiveDateTime::parse_from_str(datetime_part, "%Y%m%d%H%M%S") else { return original.to_string() };
//
//     let tz_offset_minutes = if tz_part.len() == 5 {
//         let bytes = tz_part.as_bytes();
//         let sign = if bytes.first() == Some(&b'-') { -1 } else { 1 };
//         let hours: i32 = tz_part.get(1..3).and_then(|s| s.parse().ok()).unwrap_or(0);
//         let mins: i32 = tz_part.get(3..5).and_then(|s| s.parse().ok()).unwrap_or(0);
//         sign * (hours * 60 + mins)
//     } else {
//         0
//     };
//
//     let tz = FixedOffset::east_opt(tz_offset_minutes * 60).unwrap_or(FixedOffset::east_opt(0).unwrap()); // should not panic
//
//     let dt: DateTime<FixedOffset> = tz
//         .from_local_datetime(&naive_dt)
//         .single()
//         .unwrap_or_else(|| tz.from_utc_datetime(&naive_dt));
//
//     let shifted_dt = dt + *shift;
//
//     format!("{} {}", shifted_dt.format("%Y%m%d%H%M%S"), format_offset(tz_offset_minutes))
// }

/// # Panics
/// unwrap for `FixedOffset` should not panic!
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

pub fn format_xmltv_time_utc(ts: i64, offset_minutes: i32) -> String {
    let dt = Utc.timestamp_opt(ts, 0).unwrap();
    if offset_minutes == 0 {
        dt.format("%Y%m%d%H%M%S %z").to_string()
    } else {
        match chrono::FixedOffset::east_opt(offset_minutes * 60) {
            Some(offset) => {
                dt.with_timezone(&offset).format("%Y%m%d%H%M%S %z").to_string()
            }
            None => {
                dt.format("%Y%m%d%H%M%S %z").to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeshift() {
        assert_eq!(parse_timeshift(Some(&String::from("2"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("-1:30"))), Some(-90));
        assert_eq!(parse_timeshift(Some(&String::from("+0:15"))), Some(15));
        assert_eq!(parse_timeshift(Some(&String::from("1:45"))), Some(105));
        assert_eq!(parse_timeshift(Some(&String::from(":45"))), Some(45));
        assert_eq!(parse_timeshift(Some(&String::from("-:45"))), Some(-45));
        assert_eq!(parse_timeshift(Some(&String::from("0:30"))), Some(30));
        assert_eq!(parse_timeshift(Some(&String::from(":3"))), Some(3));
        assert_eq!(parse_timeshift(Some(&String::from("2:"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("+2:00"))), Some(120));
        assert_eq!(parse_timeshift(Some(&String::from("-0:10"))), Some(-10));
        assert_eq!(parse_timeshift(Some(&String::from("invalid"))), None);
        assert_eq!(parse_timeshift(Some(&String::from("+abc"))), None);
        assert_eq!(parse_timeshift(Some(&String::new())), None);
        assert_eq!(parse_timeshift(None), None);
    }

    #[test]
    fn test_parse_timezone() {
        // This will depend on current DST; we just check it’s within a valid range
        let berlin = parse_timeshift(Some(&"Europe/Berlin".to_string())).unwrap();
        assert!(berlin == 60 || berlin == 120, "Berlin offset should be 60 or 120, got {berlin}");

        let new_york = parse_timeshift(Some(&"America/New_York".to_string())).unwrap();
        assert!(new_york == -300 || new_york == -240, "New York offset should be -300 or -240, got {new_york}");

        let tokyo = parse_timeshift(Some(&"Asia/Tokyo".to_string())).unwrap();
        assert_eq!(tokyo, 540); // always UTC+9

        let utc = parse_timeshift(Some(&"UTC".to_string())).unwrap();
        assert_eq!(utc, 0);
    }
}
