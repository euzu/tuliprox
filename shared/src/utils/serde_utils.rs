use crate::error::to_io_error;
use base64::engine::general_purpose;
use base64::Engine;
use chrono::{NaiveDateTime, ParseError, TimeZone, Utc};
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::io;
use std::sync::Arc;
use crate::utils::Internable;

fn value_to_string_array(value: &[Value]) -> Vec<Arc<str>> {
    value.iter().filter_map(value_to_arc_str).collect()
}

fn value_to_arc_str(v: &Value) -> Option<Arc<str>> {
    match v {
        Value::Bool(value) => Some(value.to_string().intern()),
        Value::Number(value) => Some(value.to_string().intern()),
        Value::String(value) => Some(value.intern()),
        _ => None,
    }
}

pub fn string_default_on_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

pub fn deserialize_as_option_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Value = serde::Deserialize::deserialize(deserializer)?;

    match &value {
        Value::String(s) => Ok(Some(s.to_owned())),
        Value::Number(s) => Ok(Some(s.to_string())),
        _ => Ok(None),
    }
}

pub fn serialize_option_string_as_null_if_empty<T, S>(
    value: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: AsRef<str>,
    S: serde::Serializer,
{
    match value {
        None => serializer.serialize_none(),
        Some(s) if s.as_ref().is_empty() => serializer.serialize_none(),
        Some(s) => serializer.serialize_str(s.as_ref()),
    }
}

pub fn deserialize_as_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Value = serde::Deserialize::deserialize(deserializer)?;

    match &value {
        Value::String(s) => Ok(s.to_string()),
        Value::Number(s) => Ok(s.to_string()),
        Value::Null => Ok(String::new()),
        _ => Ok(value.to_string()),
    }
}

pub fn deserialize_as_string_array<'de, D>(deserializer: D) -> Result<Option<Vec<Arc<str>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(|v| match v {
        Value::String(value) => Some(vec![value.intern()]),
        Value::Array(value) => Some(value_to_string_array(&value)),
        _ => None,
    })
}

pub fn deserialize_number_from_string<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
{
    let raw: Value = Value::deserialize(deserializer)?;

    match raw {
        // Null → None
        Value::Null => Ok(None),

        // its a number
        Value::Number(n) => {
            let s = n.to_string();
            if let Ok(r) = s.parse::<T>() {
                Ok(Some(r))
            } else {
                Ok(None)
            }
        }

        // String → extract first number
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }

            // Find first digit OR a '.' that is immediately followed by a digit;
            // include optional sign immediately before it (ignoring whitespace).
            let mut last_non_ws: Option<(usize, char)> = None;
            let mut num_pos: Option<usize> = None;
            for (i, c) in s.char_indices() {
                if c.is_ascii_digit() {
                    num_pos = Some(i);
                    break;
                }
                if c == '.' && s[i + 1..].chars().next().is_some_and(|n| n.is_ascii_digit()) {
                    num_pos = Some(i);
                    break;
                }
                if !c.is_whitespace() {
                    last_non_ws = Some((i, c));
                }
            }
            let Some(num_i) = num_pos else { return Ok(None); };

            let start = match last_non_ws {
                Some((i, '-')) | Some((i, '+')) => i,
                _ => num_i,
            };
            let mut it = s[start..].chars().peekable();

            // optional sign
            let mut out = String::new();
            if matches!(it.peek(), Some('-' | '+')) {
                out.push(it.next().unwrap());
                while matches!(it.peek(), Some(c) if c.is_whitespace()) {
                    it.next();
                }
            }

            // digits + optional single dot
            let mut saw_digit = false;
            let mut saw_dot = false;
            while let Some(&c) = it.peek() {
                if c.is_ascii_digit() {
                    saw_digit = true;
                    out.push(c);
                    it.next();
                    continue;
                }
                if c == '.' && !saw_dot {
                    saw_dot = true;
                    out.push(c);
                    it.next();
                    continue;
                }
                break;
            }
            if !saw_digit {
                return Ok(None);
            }

            // Try full parse; if it fails and we included '.', fall back to integer part.
            if let Ok(v) = out.parse::<T>() {
                return Ok(Some(v));
            }
            if saw_dot {
                let int_part = out.split('.').next().unwrap_or("");
                if !int_part.is_empty() && int_part != "-" && int_part != "+" {
                    if let Ok(v) = int_part.parse::<T>() {
                        return Ok(Some(v));
                    }
                }
            }
            Ok(None)
        }

        // invalid -> return None
        _ => Ok(None),
    }
}

pub fn deserialize_number_from_string_or_zero<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr + Default,
{
    match deserialize_number_from_string(deserializer) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Ok(T::default()),
        Err(e) => Err(e),
    }
}

#[inline]
pub fn bin_serialize<T>(value: &T) -> io::Result<Vec<u8>>
where
    T: serde::Serialize,
{
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).map_err(to_io_error)?;
    Ok(buf)
}

#[inline]
pub fn bin_deserialize<T>(value: &[u8]) -> io::Result<T>
where
    T: for<'a> serde::Deserialize<'a>,
{
    ciborium::de::from_reader(value).map_err(to_io_error)
}


pub fn u8_16_to_hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{:02X}", b)).collect()
}

pub fn hex_to_u8_16(hex: &str) -> Result<[u8; 16], String> {
    if hex.len() != 32 {
        return Err("Hex string must be exactly 32 characters".into());
    }

    let mut out = [0u8; 16];

    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| "Invalid UTF-8")?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| "Invalid hex")?;
    }

    Ok(out)
}

pub fn hex_to_secret<'de, D>(deserializer: D) -> Result<[u8; 16], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    hex_to_u8_16(&s).map_err(serde::de::Error::custom)
}

pub fn secret_to_hex<S>(bytes: &[u8; 16], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&u8_16_to_hex(bytes))
}

/// Deserializes a timestamp from either a Unix timestamp (seconds) or a UTC datetime string
/// in the format "YYYY-MM-DD HH:MM:SS". Note: Datetime strings are interpreted as UTC.
pub fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // - try to deserialize as seconds
    // - try to deserialize as date-time string of format like "2028-11-23 14:12:34"
    let val = Option::<Value>::deserialize(deserializer)?;
    match val {
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("invalid number"))
            .map(Some),
        Some(Value::String(s)) => parse_timestamp(&s).map_err(serde::de::Error::custom),
        Some(Value::Null) => Ok(None),
        Some(_) => Err(serde::de::Error::custom("expected number or string")),
        None => Ok(None),
    }
}

pub fn parse_timestamp(value: &str) -> Result<Option<i64>, ParseError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    if let Ok(ts) = value.parse::<i64>() {
        return Ok(Some(ts));
    }

    //  "YYYY-MM-DD HH:MM:SS"
    let dt = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")?;
    let timestamp = Utc.from_utc_datetime(&dt).timestamp();
    Ok(Some(timestamp))
}

/// Serializes an `Option<String>` as a Base64-encoded, LZ4-compressed string.
///
/// - We want to avoid using `serde_json::Value` in the struct to save memory.
/// - To avoid JSON escaping issues, we store the JSON content as a string.
/// - The string is compressed using LZ4 and encoded in Base64.
/// - Works for any JSON content: strings, arrays, and objects.
/// - Empty arrays or objects are serialized as `null`.
pub fn serialize_json_as_opt_string<S>(value: &Option<Arc<str>>,
                                       serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = match value {
        Some(s) => s,
        None => return serializer.serialize_none(),
    };

    let bytes = s.as_bytes();
    let compressed = lz4_flex::compress_prepend_size(bytes);
    let encoded = general_purpose::STANDARD_NO_PAD.encode(compressed);

    serializer.serialize_some(&encoded)
}

/// Deserializes an `Option<String>` from JSON.
///
/// - Accepts both compressed Base64-LZ4 strings and regular JSON strings.
/// - Returns `None` for empty arrays, empty objects, null, numbers, or booleans.
/// - Decompresses Base64-LZ4 content back into the original string.
/// - Handles arrays and objects by converting them to JSON strings if not empty.
pub fn deserialize_json_as_opt_string<'de, D>(
    deserializer: D,
) -> Result<Option<Arc<str>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(None),
        Some(Value::Array(arr)) if arr.is_empty() => Ok(None),
        Some(Value::Object(obj)) if obj.is_empty() => Ok(None),
        Some(Value::String(s)) => {
            if s.is_empty() {
                Ok(None)
            } else {
                let compressed = match base64::engine::general_purpose::STANDARD_NO_PAD.decode(&s) {
                    Ok(bytes) => bytes,
                    Err(_) => return Ok(Some(s.into())),
                };

                let decompressed = match lz4_flex::decompress_size_prepended(&compressed) {
                    Ok(bytes) => bytes,
                    Err(_) => return Ok(None),
                };

                match String::from_utf8(decompressed) {
                    Ok(text) => Ok(Some(text.into())),
                    Err(_) => Ok(None),
                }
            }
        }
        Some(Value::Null)
        | Some(Value::Number(_))
        | Some(Value::Bool(_)) => Ok(None),
        Some(v) => Ok(Some(serde_json::to_string(&v).map_err(D::Error::custom)?.into())),
    }
}

/// Serialize an Option<Vec<T>> in flow-style YAML (- { key: value, ... })
pub fn serialize_option_slice_flow<T, S>(
    opt: &Option<Vec<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: serde::Serialize,
    S: serde::Serializer,
{
    match opt {
        Some(items) if !items.is_empty() => {
            serde_saphyr::FlowSeq(items).serialize(serializer)
        }
        _ => serializer.serialize_none(),
    }
}

/// Serialize an Option<Vec<T>> as a block sequence where each item is in flow-style
pub fn serialize_option_vec_flow_map_items<T, S>(
    opt: &Option<Vec<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: serde::Serialize,
    S: serde::Serializer,
{
    match opt {
        Some(items) if !items.is_empty() => {
            let flow_items: Vec<_> = items.iter().map(serde_saphyr::FlowMap).collect();
            flow_items.serialize(serializer)
        }
        _ => serializer.serialize_none(),
    }
}

pub fn serialize_number_as_string<N, S>(value: &N, serializer: S) -> Result<S::Ok, S::Error>
where
    N: std::fmt::Display,
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}


/// Serde support for `XtreamCluster` fields.
/// Serializes as string (e.g., "live", "video", "series") and deserializes via `FromStr`.
pub mod xtream_cluster_serde {
    use std::str::FromStr;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde::de::Error;
    use crate::model::XtreamCluster;

    pub fn serialize<S>(value: &XtreamCluster, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<XtreamCluster, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        XtreamCluster::from_str(&raw).map_err(D::Error::custom)
    }
}