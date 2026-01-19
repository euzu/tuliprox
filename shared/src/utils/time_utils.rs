#[cfg(target_arch = "wasm32")]
pub fn current_time_secs() -> u64 {
    (js_sys::Date::now() / 1000.0) as u64
}

#[cfg(not(target_arch = "wasm32"))]
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn unix_ts_to_str(ts: i64) -> Option<String> {
    unix_ts_to_str_with_format(ts, "%Y-%m-%d %H:%M:%S")
}

fn normalize_ts(ts: i64) -> Option<i64> {
    if ts > 0 {
        if ts > 4_102_444_800 {
            Some(ts / 1000)
        } else {
            Some(ts)
        }
    } else {
        None
    }
}

#[cfg(target_arch = "wasm32")]
pub fn unix_ts_to_str_with_format(ts: i64, _format: &str) -> Option<String> {
    let normalized_ts = normalize_ts(ts)?;
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(
        normalized_ts as f64 * 1000.0,
    ));
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    ))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn unix_ts_to_str_with_format(ts: i64, format: &str) -> Option<String> {
    let normalized_ts = normalize_ts(ts)?;
    chrono::DateTime::from_timestamp(normalized_ts, 0).map(|dt| dt.format(format).to_string())
}
