use super::is_date_only_yyyy_mm_dd;
use crate::api::model::AppState;
use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use shared::{error::TuliproxError, utils::parse_timestamp};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PanelApiExpireMode {
    UtcString,
    ServerTzString,
}

#[derive(Debug, Clone)]
pub(super) struct PanelApiTimeContext {
    pub(super) expire_mode: PanelApiExpireMode,
    pub(super) server_tz: Option<Tz>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PanelApiTimeCache {
    pub(super) inputs: HashMap<String, PanelApiTimeCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PanelApiTimeCacheEntry {
    pub(super) expire_mode: PanelApiExpireMode,
    pub(super) server_tz: Option<String>,
    pub(super) skew_secs: Option<i64>,
}

pub(super) fn parse_panel_expire_utc(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = parse_timestamp(value).ok().flatten();
    if parsed.is_some() {
        return parsed;
    }
    if is_date_only_yyyy_mm_dd(value) {
        let normalized = format!("{value} 00:00:00");
        return parse_timestamp(&normalized).ok().flatten();
    }
    None
}

pub(super) fn parse_panel_expire_with_tz(value: &str, tz: Tz) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(ts) = value.parse::<i64>() {
        return Some(ts);
    }
    let value = if is_date_only_yyyy_mm_dd(value) { format!("{value} 00:00:00") } else { value.to_string() };
    let dt = NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S").ok()?;
    match tz.from_local_datetime(&dt) {
        chrono::LocalResult::Single(local_dt) => Some(local_dt.timestamp()),
        chrono::LocalResult::Ambiguous(first, _) => Some(first.timestamp()),
        chrono::LocalResult::None => None,
    }
}

pub(super) fn normalize_panel_expire(value: &str, ctx: Option<&PanelApiTimeContext>) -> Option<i64> {
    let Some(ctx) = ctx else {
        return parse_panel_expire_utc(value);
    };
    if let Ok(ts) = value.trim().parse::<i64>() {
        return Some(ts);
    }
    match ctx.expire_mode {
        PanelApiExpireMode::UtcString => parse_panel_expire_utc(value),
        PanelApiExpireMode::ServerTzString => {
            ctx.server_tz.and_then(|tz| parse_panel_expire_with_tz(value, tz)).or_else(|| parse_panel_expire_utc(value))
        }
    }
}

pub(super) fn is_expiring_with_offset_at(exp_date: Option<i64>, offset_secs: u64, now: u64) -> bool {
    let Some(exp_date) = exp_date else {
        return false;
    };
    let Ok(exp_ts) = u64::try_from(exp_date) else {
        return true;
    };
    if exp_ts <= now {
        return false;
    }
    now.saturating_add(offset_secs) >= exp_ts
}

pub(super) fn resolve_panel_expire_mode(
    root_expire: Option<i64>,
    panel_expire: Option<&str>,
    server_tz: Option<Tz>,
) -> PanelApiExpireMode {
    let Some(root_expire) = root_expire else {
        return PanelApiExpireMode::UtcString;
    };
    let Some(panel_expire) = panel_expire else {
        return PanelApiExpireMode::UtcString;
    };
    let utc_ts = parse_panel_expire_utc(panel_expire);
    let tz_ts = server_tz.and_then(|tz| parse_panel_expire_with_tz(panel_expire, tz));
    let Some(utc_ts) = utc_ts else {
        return tz_ts.map_or(PanelApiExpireMode::UtcString, |_| PanelApiExpireMode::ServerTzString);
    };
    let Some(tz_ts) = tz_ts else {
        return PanelApiExpireMode::UtcString;
    };
    let diff_utc = (utc_ts - root_expire).abs();
    let diff_tz = (tz_ts - root_expire).abs();
    let threshold = 120_i64;
    match (diff_utc <= threshold, diff_tz <= threshold) {
        (true, false) => PanelApiExpireMode::UtcString,
        (false, true) => PanelApiExpireMode::ServerTzString,
        _ => {
            if diff_tz < diff_utc {
                PanelApiExpireMode::ServerTzString
            } else {
                PanelApiExpireMode::UtcString
            }
        }
    }
}

pub(super) fn apply_clock_skew(now: u64, skew_secs: i64) -> u64 {
    if skew_secs == 0 {
        return now;
    }
    if skew_secs.is_negative() {
        now.saturating_sub(skew_secs.unsigned_abs())
    } else {
        now.saturating_add(u64::try_from(skew_secs).unwrap_or(0))
    }
}

pub(super) fn panel_api_time_cache_path(app_state: &AppState) -> PathBuf {
    let paths = app_state.app_config.paths.load();
    PathBuf::from(&paths.config_path).join("panel-api").join("panel_api_time_cache.json")
}

pub(super) async fn load_panel_api_time_cache(app_state: &AppState, cache_path: &Path) -> PanelApiTimeCache {
    if let Some(parent) = cache_path.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return PanelApiTimeCache::default();
        }
    }
    let _lock = app_state.app_config.file_locks.read_lock(cache_path).await;
    let Ok(content) = tokio::fs::read_to_string(cache_path).await else {
        return PanelApiTimeCache::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub(super) async fn persist_panel_api_time_cache(
    app_state: &AppState,
    cache_path: &Path,
    cache: &PanelApiTimeCache,
) -> Result<(), TuliproxError> {
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api: failed to create cache dir: {e}")))?;
    }
    let content =
        serde_json::to_string_pretty(cache).map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api: {e}")))?;
    let _lock = app_state.app_config.file_locks.write_lock(cache_path).await;
    tokio::fs::write(cache_path, content)
        .await
        .map_err(|e| TuliproxError::ConfigPanelApi(format!("panel_api: failed to persist time cache: {e}")))?;
    Ok(())
}

pub(super) fn parse_cached_tz(tz: Option<String>) -> Option<Tz> { tz.and_then(|name| name.parse::<Tz>().ok()) }
