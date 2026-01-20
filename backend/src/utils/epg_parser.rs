use quick_xml::events::{Event, BytesStart};
use quick_xml::reader::Reader;
use tokio::io::{AsyncRead, AsyncBufRead, BufReader};
use shared::error::{TuliproxError, info_err};
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, TimeZone};
use crate::utils::{obscure_text};
use crate::model::{EPG_TAG_ICON, EPG_TAG_PROGRAMME};
use log::error;

pub trait EpgConsumer: Send {
    fn handle_event(&mut self, event: &Event<'_>, decoder: quick_xml::Decoder) -> impl std::future::Future<Output = Result<(), TuliproxError>> + Send;
}

pub struct EpgProcessor<R: AsyncBufRead + Send + Unpin> {
    reader: Reader<R>,
    offset_minutes: i32,
    rewrite_urls: bool,
    rewrite_base_url: String,
    encrypt_secret: [u8; 16],
    filter_channel_id: Option<std::sync::Arc<str>>,
}

impl<R: AsyncRead + Send + Unpin> EpgProcessor<BufReader<R>> {
    pub fn new(
        reader: R,
        offset_minutes: i32,
        rewrite_urls: bool,
        rewrite_base_url: String,
        encrypt_secret: [u8; 16],
        filter_channel_id: Option<std::sync::Arc<str>>,
    ) -> Self {
        Self {
            reader: Reader::from_reader(BufReader::new(reader)),
            offset_minutes,
            rewrite_urls,
            rewrite_base_url,
            encrypt_secret,
            filter_channel_id,
        }
    }
}

#[allow(clippy::too_many_lines)]
impl<R: AsyncBufRead + Send + Unpin> EpgProcessor<R> {
    pub async fn process<C: EpgConsumer>(&mut self, consumer: &mut C) -> Result<(), TuliproxError> {
        let mut buf = Vec::with_capacity(4096);
        let duration = Duration::minutes(i64::from(self.offset_minutes));
        let mut skip_depth = None;

        loop {
            buf.clear();
            let event = match self.reader.read_event_into_async(&mut buf).await {
                Ok(e) => e,
                Err(e) => {
                    error!("Error reading epg XML event: {e}");
                    return Err(info_err!("Error reading epg XML event: {}", e));
                }
            };

            if let Some(flt) = &self.filter_channel_id {
                match &event {
                    Event::Start(e) => {
                        if skip_depth.is_none() {
                            let should_skip = match e.name().as_ref() {
                                b"channel" => {
                                    e.attributes()
                                        .filter_map(Result::ok)
                                        .find(|a| a.key.as_ref() == b"id")
                                        .and_then(|a| a.unescape_value().ok())
                                        .is_some_and(|v| flt.as_ref() != v.as_ref())
                                }
                                b"programme" => {
                                    e.attributes()
                                        .filter_map(Result::ok)
                                        .find(|a| a.key.as_ref() == b"channel")
                                        .and_then(|a| a.unescape_value().ok())
                                        .is_some_and(|v| flt.as_ref() != v.as_ref())
                                }
                                _ => false,
                            };

                            if should_skip {
                                skip_depth = Some(1);
                                continue;
                            }
                        } else {
                            skip_depth = skip_depth.map(|d| d + 1);
                            continue;
                        }
                    }
                    Event::End(_) => {
                        if let Some(depth) = skip_depth {
                            if depth == 1 {
                                skip_depth = None;
                            } else {
                                skip_depth = Some(depth - 1);
                            }
                            continue;
                        }
                    }
                    Event::Empty(_) => {
                        if skip_depth.is_some() {
                            continue;
                        }
                    }
                    _ => {}
                }

                if skip_depth.is_some() {
                    continue;
                }
            }

            match event {
                Event::Start(ref e) if self.offset_minutes != 0 && e.name().as_ref() == b"programme" => {
                    let mut elem = BytesStart::new(EPG_TAG_PROGRAMME);
                    for attr in e.attributes() {
                        match attr {
                            Ok(attr) if attr.key.as_ref() == b"start" => {
                                if let Ok(start_value) = attr.decode_and_unescape_value(self.reader.decoder()) {
                                    elem.push_attribute(("start", time_correct(&start_value, &duration).as_str()));
                                } else {
                                    elem.push_attribute(attr);
                                }
                            }
                            Ok(attr) if attr.key.as_ref() == b"stop" => {
                                if let Ok(stop_value) = attr.decode_and_unescape_value(self.reader.decoder()) {
                                    elem.push_attribute(("stop", time_correct(&stop_value, &duration).as_str()));
                                } else {
                                    elem.push_attribute(attr);
                                }
                            }
                            Ok(attr) => {
                                elem.push_attribute(attr);
                            }
                            Err(e) => {
                                error!("Error parsing epg attribute: {e}");
                            }
                        }
                    }
                    consumer.handle_event(&Event::Start(elem), self.reader.decoder()).await?;
                }
                ref event @ (Event::Empty(ref e) | Event::Start(ref e)) if self.rewrite_urls && e.name().as_ref() == b"icon" => {
                    let mut elem = BytesStart::new(EPG_TAG_ICON);
                    for attr in e.attributes() {
                        match attr {
                            Ok(attr) if attr.key.as_ref() == b"src" => {
                                if let Some(icon) = get_attr_value_unescaped(&attr, self.reader.decoder()) {
                                    if icon.is_empty() {
                                        elem.push_attribute(attr);
                                    } else {
                                        let rewritten_url = if let Ok(encrypted) = obscure_text(&self.encrypt_secret, &icon) {
                                            format!("{}{}", self.rewrite_base_url, encrypted)
                                        } else {
                                            icon
                                        };
                                        elem.push_attribute(("src", rewritten_url.as_str()));
                                    }
                                } else {
                                    elem.push_attribute(attr);
                                }
                            }
                            Ok(attr) => {
                                elem.push_attribute(attr);
                            }
                            Err(e) => {
                                error!("Error parsing epg attribute: {e}");
                            }
                        }
                    }

                    let out_event = match event {
                        Event::Empty(_) => Event::Empty(elem),
                        Event::Start(_) => Event::Start(elem),
                        _ => unreachable!(),
                    };
                    consumer.handle_event(&out_event, self.reader.decoder()).await?;
                }
                Event::Decl(_) | Event::DocType(_) => {},
                Event::Eof => break,
                ref event => {
                    consumer.handle_event(event, self.reader.decoder()).await?;
                }
            }
        }
        Ok(())
    }
}

/// # Panics
/// unwrap for `FixedOffset` should not panic!
pub fn time_correct(original: &str, shift: &Duration) -> String {
    let (datetime_part, tz_part) = if let Some((dt, tz)) = original.trim().rsplit_once(' ') {
        (dt, tz)
    } else {
        (original.trim(), "+0000")
    };

    let Ok(naive_dt) = NaiveDateTime::parse_from_str(datetime_part, "%Y%m%d%H%M%S") else { return original.to_string() };

    let tz_offset_minutes = if tz_part.len() == 5 {
        let sign = if &tz_part[0..1] == "-" { -1 } else { 1 };
        let hours: i32 = tz_part[1..3].parse().unwrap_or(0);
        let mins: i32 = tz_part[3..5].parse().unwrap_or(0);
        sign * (hours * 60 + mins)
    } else {
        0
    };

    let tz = FixedOffset::east_opt(tz_offset_minutes * 60).unwrap_or(FixedOffset::east_opt(0).unwrap()); // should not panic

    let dt: DateTime<FixedOffset> = tz
        .from_local_datetime(&naive_dt)
        .single()
        .unwrap_or_else(|| tz.from_utc_datetime(&naive_dt));

    let shifted_dt = dt + *shift;

    format!("{} {}", shifted_dt.format("%Y%m%d%H%M%S"), format_offset(tz_offset_minutes))
}

pub fn format_offset(offset_minutes: i32) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let abs = offset_minutes.abs();
    let hours = abs / 60;
    let mins = abs % 60;
    format!("{sign}{hours:02}{mins:02}")
}
fn get_attr_value_unescaped(attr: &quick_xml::events::attributes::Attribute, decoder: quick_xml::Decoder) -> Option<String> {
    attr.decode_and_unescape_value(decoder).ok().map(|v| v.to_string())
}

pub fn format_xtream_time(ts: i64) -> String {
    if let Some(dt) = DateTime::from_timestamp(ts, 0) {
        dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        String::new()
    }
}
