use futures::StreamExt;
use log::warn;
use serde::{Deserialize, Serialize};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use shared::utils::deserialize_as_option_string;
use std::fmt;
use std::io::{Error as IoError, ErrorKind, Read};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tokio_util::io::{StreamReader, SyncIoBridge};

use crate::iptv::stalker::client::StalkerApiClient;
use crate::iptv::stalker::error::{StalkerError, StalkerResult};
use crate::iptv::stalker::profile::StalkerHandshake;
use crate::iptv::stalker::recipes::recipe_spec_for;

/// A single EPG programme record. The portal wraps each entry in `{ ch_id, title, start,
/// stop, ... }`. We accept any payload shape by deserialising into a permissive value
/// first and then coercing the bits we actually need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalkerProgramRecord {
    pub channel_id: Option<String>,
    pub title: String,
    pub start_epoch: Option<i64>,
    pub stop_epoch: Option<i64>,
    pub description: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct RawStalkerProgram {
    #[serde(default)]
    ch_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    start: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    stop: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    start_timestamp: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_string")]
    stop_timestamp: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    t_time: Option<String>,
    #[serde(default)]
    p_time: Option<String>,
}

impl RawStalkerProgram {
    fn into_record(self) -> Option<StalkerProgramRecord> {
        let title = self.title.or(self.name)?;
        let start_epoch = self
            .start_timestamp
            .as_deref()
            .and_then(parse_stalker_timestamp)
            .or_else(|| self.start.as_deref().and_then(parse_stalker_timestamp));
        let stop_epoch = self
            .stop_timestamp
            .as_deref()
            .and_then(parse_stalker_timestamp)
            .or_else(|| self.stop.as_deref().and_then(parse_stalker_timestamp));
        Some(StalkerProgramRecord {
            channel_id: self.ch_id.or(self.id),
            title,
            start_epoch,
            stop_epoch,
            description: self.desc.or(self.description),
            category: self.category.or(self.t_time).or(self.p_time),
        })
    }
}

fn parse_stalker_timestamp(raw: &str) -> Option<i64> {
    if let Ok(value) = raw.trim().parse::<i64>() {
        return Some(value);
    }
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.timestamp())
}

/// Short EPG (per channel, 2-24 hour window). Returns the parsed records on success.
pub async fn get_short_epg(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    channel_id: u32,
    hours: u32,
) -> StalkerResult<Vec<StalkerProgramRecord>> {
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let mut builder = client
            .http()
            .get(&load_url.load_url)
            .headers(client.common_headers(&load_url))
            .query(&[
                ("type", "itv"),
                ("action", "get_short_epg"),
                ("ch_id", &channel_id.to_string()),
                ("period", &hours.to_string()),
            ]);
        builder = client.apply_mac_query(builder);
        builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
        match client.send_json::<Value>(builder, "get_short_epg").await {
            Ok(value) => {
                return Ok(parse_epg_records(&value));
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: client.portal_url().to_string() }))
}

/// Per-channel full EPG. The portal returns the same payload shape as the short EPG.
pub async fn get_epg(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    channel_id: u32,
    period_hours: u32,
) -> StalkerResult<Vec<StalkerProgramRecord>> {
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let mut builder = client
            .http()
            .get(&load_url.load_url)
            .headers(client.common_headers(&load_url))
            .query(&[
                ("type", "itv"),
                ("action", "get_epg_info"),
                ("ch_id", &channel_id.to_string()),
                ("period", &period_hours.to_string()),
            ]);
        builder = client.apply_mac_query(builder);
        builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
        match client.send_json::<Value>(builder, "get_epg").await {
            Ok(value) => {
                return Ok(parse_epg_records(&value));
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: client.portal_url().to_string() }))
}

/// Bulk-EPG stream. The portal can return hundreds of thousands of records in a single
/// `get_epg_info?period=<h>` call, so we stream the HTTP body through a reader-backed
/// JSON deserializer and emit one programme record at a time.
pub async fn stream_bulk_epg<F, Fut>(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    period_hours: u32,
    batch_size: usize,
    mut on_batch: F,
) -> StalkerResult<()>
where
    F: FnMut(Vec<StalkerProgramRecord>) -> Fut,
    Fut: std::future::Future<Output = StalkerResult<()>>,
{
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let mut builder = client
            .http()
            .get(&load_url.load_url)
            .headers(client.common_headers(&load_url))
            .query(&[
                ("type", "itv"),
                ("action", "get_epg_info"),
                ("period", &period_hours.to_string()),
            ]);
        builder = client.apply_mac_query(builder);
        builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
        let response = match client.send_with_cap(builder, "get_epg_bulk", client.body_caps().get_epg_bytes).await {
            Ok(r) => r,
            Err(err) => {
                last_err = Some(err);
                continue;
            }
        };
        if !response.status().is_success() {
            last_err = Some(StalkerError::BadStatus {
                status: response.status().as_u16(),
                action: "get_epg_bulk".to_string(),
                body_snippet: String::new(),
            });
            continue;
        }
        client.ingest_response_cookies(&response);
        let cap = client.body_caps().get_epg_bytes;
        let mut received = 0_u64;
        let body_stream = response.bytes_stream().map(move |result| match result {
            Ok(chunk) => {
                received = received.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                if received > cap {
                    Err(IoError::other("stalker bulk EPG body exceeded configured cap"))
                } else {
                    Ok(chunk)
                }
            }
            Err(err) => Err(IoError::other(err.without_url())),
        });
        let reader = StreamReader::new(body_stream);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut cancellation_guard = EpgCancellationGuard { cancelled: Arc::clone(&cancelled), armed: true };
        let bridge = CancellableReader { inner: SyncIoBridge::new(reader), cancelled };
        // Bounded async channel between the blocking parser and the async consumer.
        // The parser side lives on a `spawn_blocking` thread, so blocking that thread
        // with `blocking_send` is the correct backpressure primitive: a full channel
        // pauses the parser until the consumer catches up, and no record is ever
        // dropped. A send error means the receiver was dropped; the serde walk cannot
        // be aborted mid-stream, so we just stop forwarding records.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StalkerProgramRecord>(256);
        let parse_task = tokio::task::spawn_blocking(move || {
            let mut receiver_closed = false;
            stream_bulk_epg_from_reader(bridge, &mut |record| {
                if receiver_closed {
                    return;
                }
                if tx.blocking_send(record).is_err() {
                    receiver_closed = true;
                }
            })
        });
        let mut emitted = 0_u64;
        let mut batch = Vec::with_capacity(batch_size.max(1));
        while let Some(record) = rx.recv().await {
            emitted = emitted.saturating_add(1);
            batch.push(record);
            if batch.len() >= batch_size.max(1) {
                on_batch(std::mem::take(&mut batch)).await?;
                batch.reserve(batch_size.max(1));
            }
        }
        if !batch.is_empty() {
            on_batch(batch).await?;
        }
        parse_task
            .await
            .map_err(|err| StalkerError::BodyDecode {
                message: format!("get_epg_bulk parser join error: {err}"),
            })?
            .map_err(|err| match err.kind() {
                ErrorKind::UnexpectedEof => StalkerError::EmptyBody {
                    action: "get_epg_bulk".to_string(),
                },
                _ => StalkerError::BodyDecode {
                    message: format!("get_epg_bulk json decode: {err}"),
                },
            })?;
        cancellation_guard.disarm();
        if emitted == 0 {
            warn!("Stalker bulk EPG returned zero programs for period {period_hours}");
        }
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: client.portal_url().to_string() }))
}

struct CancellableReader<R> {
    inner: R,
    cancelled: Arc<AtomicBool>,
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, IoError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(IoError::new(ErrorKind::Interrupted, "Stalker EPG parsing cancelled"));
        }
        self.inner.read(buffer)
    }
}

struct EpgCancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl EpgCancellationGuard {
    fn disarm(&mut self) { self.armed = false; }
}

impl Drop for EpgCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn parse_epg_records(value: &Value) -> Vec<StalkerProgramRecord> {
    let mut out = Vec::new();
    for_each_program(value, &mut |record| out.push(record));
    out
}

fn for_each_program<F>(value: &Value, on_each: &mut F)
where
    F: FnMut(StalkerProgramRecord),
{
    let candidates: Vec<Value> = match value {
        Value::Object(map) => {
            let mut v = Vec::new();
            if let Some(js) = map.get("js") {
                v.push(js.clone());
            }
            for (k, val) in map {
                if k == "js" {
                    continue;
                }
                v.push(val.clone());
            }
            v
        }
        Value::Array(arr) => vec![Value::Array(arr.clone())],
        _ => vec![],
    };
    for v in candidates {
        match v {
            Value::Array(arr) => {
                for entry in arr {
                    if let Some(record) = raw_to_record(&entry) {
                        on_each(record);
                    }
                }
            }
            Value::Object(map) => {
                if let Some(items) = map.get("data") {
                    for_each_program(items, on_each);
                } else if let Some(record) = raw_to_record(&Value::Object(map)) {
                    on_each(record);
                }
            }
            _ => {}
        }
    }
}

fn raw_to_record(value: &Value) -> Option<StalkerProgramRecord> {
    let raw: RawStalkerProgram = serde_json::from_value(value.clone()).ok()?;
    raw.into_record()
}

struct ProgramSeed<'a, F> {
    on_program: &'a mut F,
    emitted: &'a mut u64,
}

impl<'de, F> DeserializeSeed<'de> for ProgramSeed<'_, F>
where
    F: FnMut(StalkerProgramRecord),
{
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ProgramVisitor {
            on_program: self.on_program,
            emitted: self.emitted,
        })
    }
}

struct ProgramVisitor<'a, F> {
    on_program: &'a mut F,
    emitted: &'a mut u64,
}

impl<'de, F> Visitor<'de> for ProgramVisitor<'_, F>
where
    F: FnMut(StalkerProgramRecord),
{
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Stalker bulk EPG JSON document")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq
            .next_element_seed(ProgramSeed {
                on_program: self.on_program,
                emitted: self.emitted,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut raw = RawStalkerProgram::default();
        let mut saw_nested = false;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "js" | "data" => {
                    saw_nested = true;
                    map.next_value_seed(ProgramSeed {
                        on_program: self.on_program,
                        emitted: self.emitted,
                    })?;
                }
                "ch_id" => raw.ch_id = map.next_value()?,
                "id" => raw.id = map.next_value()?,
                "title" => raw.title = map.next_value()?,
                "name" => raw.name = map.next_value()?,
                "desc" => raw.desc = map.next_value()?,
                "description" => raw.description = map.next_value()?,
                "start" => raw.start = value_as_string(map.next_value()?),
                "stop" => raw.stop = value_as_string(map.next_value()?),
                "start_timestamp" => raw.start_timestamp = value_as_string(map.next_value()?),
                "stop_timestamp" => raw.stop_timestamp = value_as_string(map.next_value()?),
                "category" => raw.category = map.next_value()?,
                "t_time" => raw.t_time = map.next_value()?,
                "p_time" => raw.p_time = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        if !saw_nested {
            if let Some(record) = raw.into_record() {
                *self.emitted = self.emitted.saturating_add(1);
                (self.on_program)(record);
            }
        }
        Ok(())
    }

    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _v: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }
}

fn value_as_string(value: Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

struct LeadingJsonReader<R> {
    inner: R,
    started: bool,
    saw_non_ws: bool,
}

impl<R> LeadingJsonReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            started: false,
            saw_non_ws: false,
        }
    }
}

impl<R: Read> Read for LeadingJsonReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.started {
            return self.inner.read(buf);
        }

        loop {
            let mut one = [0_u8; 1];
            let read = self.inner.read(&mut one)?;
            if read == 0 {
                return Ok(0);
            }
            let byte = one[0];
            if !self.saw_non_ws && byte.is_ascii_whitespace() {
                continue;
            }
            if !self.saw_non_ws {
                self.saw_non_ws = true;
                if matches!(byte, 0xEF | 0xBB | 0xBF) {
                    continue;
                }
                if byte == b'<' {
                    return Err(IoError::new(ErrorKind::InvalidData, "stalker portal returned HTML instead of JSON"));
                }
            }
            if matches!(byte, b'{' | b'[') {
                self.started = true;
                buf[0] = byte;
                return Ok(1);
            }
        }
    }
}

fn stream_bulk_epg_from_reader<R, F>(reader: R, on_program: &mut F) -> Result<u64, IoError>
where
    R: Read,
    F: FnMut(StalkerProgramRecord),
{
    let mut reader = LeadingJsonReader::new(reader);
    let mut emitted = 0_u64;
    let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
    ProgramSeed {
        on_program,
        emitted: &mut emitted,
    }
    .deserialize(&mut deserializer)
    .map_err(IoError::other)?;
    Ok(emitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_stalker_timestamp_iso() {
        let parsed = parse_stalker_timestamp("2026-06-10T12:00:00Z").expect("ok");
        assert!(parsed > 0);
    }

    #[test]
    fn parses_stalker_timestamp_unix() {
        let parsed = parse_stalker_timestamp("1700000000").expect("ok");
        assert_eq!(parsed, 1_700_000_000);
    }

    #[test]
    fn parse_epg_records_unwraps_js_array() {
        let v: Value = serde_json::from_str(
            r#"{"js":[{"ch_id":"1","title":"A","start":"1700000000","stop":"1700003600"}]}"#,
        )
        .unwrap();
        let recs = parse_epg_records(&v);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].title, "A");
        assert_eq!(recs[0].start_epoch, Some(1_700_000_000));
    }

    #[test]
    fn numeric_timestamps_work_in_value_and_streaming_paths() -> Result<(), IoError> {
        let value = serde_json::json!({
            "js": [{"ch_id": "1", "title": "A", "start": 1_700_000_000, "stop_timestamp": 1_700_003_600}]
        });
        let records = parse_epg_records(&value);
        assert_eq!(records[0].start_epoch, Some(1_700_000_000));
        assert_eq!(records[0].stop_epoch, Some(1_700_003_600));

        let mut streamed = Vec::new();
        let emitted = stream_bulk_epg_from_reader(
            Cursor::new(value.to_string()),
            &mut |record| streamed.push(record),
        )?;
        assert_eq!(emitted, 1);
        assert_eq!(streamed, records);
        Ok(())
    }

    #[test]
    fn for_each_program_visits_object_data() {
        let v: Value = serde_json::from_str(
            r#"{"js":{"data":[{"title":"x"}]}}"#,
        )
        .unwrap();
        let mut count = 0;
        for_each_program(&v, &mut |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn stream_bulk_epg_from_reader_walks_js_data_without_materializing_value_tree() {
        let json = r#"{"js":{"data":[{"ch_id":"1","title":"A","start":"1700000000","stop":"1700003600"},{"ch_id":"2","title":"B","start":"1700003600","stop":"1700007200"}]}}"#;
        let mut records = Vec::new();
        let emitted = stream_bulk_epg_from_reader(Cursor::new(json.as_bytes()), &mut |record| records.push(record)).expect("stream parse");
        assert_eq!(emitted, 2);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].channel_id.as_deref(), Some("1"));
        assert_eq!(records[1].title, "B");
    }

    #[test]
    fn stream_bulk_epg_from_reader_accepts_jsonp_and_bom_prefix() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(br#"callback({"js":[{"ch_id":"9","title":"Wrapped","start":"1700000000","stop":"1700003600"}]})"#);
        let mut records = Vec::new();
        let emitted = stream_bulk_epg_from_reader(Cursor::new(body), &mut |record| records.push(record)).expect("stream parse");
        assert_eq!(emitted, 1);
        assert_eq!(records[0].title, "Wrapped");
    }
}
