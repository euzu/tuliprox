use super::{
    default_recording_filename_template, default_recording_timezone, defaults::MAX_FILENAME_TEMPLATE_BYTES,
    RecordingConfigDto,
};
use crate::{
    defaults::{
        default_episode_pattern, default_recording_dir, is_blank_or_default_episode_pattern, DEFAULT_USER_AGENT,
    },
    error::TuliproxError,
};

/// Allowed placeholders in the recording filename template.
/// Order matters for error messages and the validation regex.
pub const RECORDING_FILENAME_PLACEHOLDERS: &[&str] =
    &["{channel}", "{program_title}", "{start_time}", "{end_time}", "{episode}", "{owner}"];

/// Validates a recording filename template. Returns the cleaned template and
/// the matched placeholder set on success.
fn validate_recording_filename_template(template: &str) -> Result<(), TuliproxError> {
    if template.is_empty() {
        return Err(TuliproxError::ConfigRecording("recording.filename_template must not be empty".to_string()));
    }
    if template.len() > MAX_FILENAME_TEMPLATE_BYTES {
        return Err(TuliproxError::ConfigRecording(format!(
            "recording.filename_template must not exceed {MAX_FILENAME_TEMPLATE_BYTES} bytes"
        )));
    }

    let bytes = template.as_bytes();
    let mut i = 0;
    let mut found_placeholder = false;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close) = template[i + 1..].find('}') {
                let end = i + 1 + close;
                let placeholder = &template[i..=end];
                if !RECORDING_FILENAME_PLACEHOLDERS.contains(&placeholder) {
                    return Err(TuliproxError::ConfigRecording(format!(
                        "recording.filename_template contains unknown placeholder '{placeholder}'"
                    )));
                }
                found_placeholder = true;
                i = end + 1;
            } else {
                return Err(TuliproxError::ConfigRecording(
                    "recording.filename_template has an unmatched '{'".to_string(),
                ));
            }
        } else if bytes[i] == b'}' {
            return Err(TuliproxError::ConfigRecording("recording.filename_template has an unmatched '}'".to_string()));
        } else {
            i += 1;
        }
    }

    if !found_placeholder {
        return Err(TuliproxError::ConfigRecording(
            "recording.filename_template must contain at least one placeholder".to_string(),
        ));
    }
    Ok(())
}

fn validate_recording_timezone(tz: &str) -> Result<(), TuliproxError> {
    if tz.is_empty() {
        return Err(TuliproxError::ConfigRecording("recording.timezone must not be empty".to_string()));
    }
    tz.parse::<chrono_tz::Tz>()
        .map(|_| ())
        .map_err(|_| TuliproxError::ConfigRecording(format!("recording.timezone '{tz}' is not a valid IANA timezone")))
}

pub(crate) fn prepare_recording_config(recording: &mut RecordingConfigDto) -> Result<(), TuliproxError> {
    if recording.headers.is_empty() {
        recording.headers.insert("Accept".to_string(), "video/*".to_string());
        recording.headers.insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());
    }
    if is_blank_or_default_episode_pattern(&recording.episode_pattern) {
        recording.episode_pattern = default_episode_pattern();
    } else if let Some(pattern) = recording.episode_pattern.as_ref() {
        recording.episode_pattern = Some(pattern.trim().to_string());
    }
    if let Some(pattern) = recording.episode_pattern.as_ref() {
        crate::model::REGEX_CACHE
            .get_or_compile(pattern)
            .map_err(|err| TuliproxError::RegexCompile(format!("{pattern} {err}")))?;
    }
    recording.retry_backoff_initial_secs = recording.retry_backoff_initial_secs.max(1);
    recording.retry_backoff_multiplier = recording.retry_backoff_multiplier.max(1.0);
    recording.retry_backoff_max_secs = recording.retry_backoff_max_secs.max(recording.retry_backoff_initial_secs);
    recording.retry_backoff_jitter_percent = recording.retry_backoff_jitter_percent.min(95);
    recording.retry_max_attempts = recording.retry_max_attempts.max(1);

    // Directory is independent of video extensions and download settings.
    if let Some(dir) = recording.directory.as_ref() {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            recording.directory = None;
        } else if trimmed != dir {
            recording.directory = Some(trimmed.to_string());
        }
    }
    if recording.directory.is_none() {
        recording.directory = default_recording_dir();
    }

    // timezone: default UTC; validate IANA.
    if let Some(tz) = recording.timezone.as_ref() {
        let trimmed = tz.trim();
        if trimmed.is_empty() {
            recording.timezone = None;
        } else if trimmed != tz {
            recording.timezone = Some(trimmed.to_string());
        }
    }
    if let Some(tz) = recording.timezone.as_ref() {
        if tz.parse::<chrono_tz::Tz>().is_err() {
            // Surface a warning before the strict validator below turns
            // this into a hard error, so the operator sees both signals.
            log::warn!("recording.timezone '{tz}' is not a valid IANA timezone");
        }
    }
    if recording.timezone.is_none() {
        recording.timezone = Some(default_recording_timezone());
    }
    validate_recording_timezone(recording.timezone.as_deref().unwrap_or("UTC"))?;

    // filename_template: default; validate placeholders.
    if let Some(template) = recording.filename_template.as_ref() {
        let trimmed = template.trim();
        if trimmed != template {
            recording.filename_template = Some(trimmed.to_string());
        }
    }
    if recording.filename_template.is_none() {
        recording.filename_template = Some(default_recording_filename_template());
    }
    validate_recording_filename_template(recording.filename_template.as_deref().unwrap_or(""))?;

    // padding: default <= max, fallback to defaults if missing.
    if recording.default_pre_roll_secs.is_none() {
        recording.default_pre_roll_secs = Some(0);
    }
    if recording.default_pre_roll_secs.unwrap_or(0) > recording.max_pre_roll_secs {
        return Err(TuliproxError::ConfigRecording(format!(
            "recording.default_pre_roll_secs ({}) must not exceed max_pre_roll_secs ({})",
            recording.default_pre_roll_secs.unwrap_or(0),
            recording.max_pre_roll_secs
        )));
    }
    if recording.default_post_roll_secs.is_none() {
        recording.default_post_roll_secs = Some(0);
    }
    if recording.default_post_roll_secs.unwrap_or(0) > recording.max_post_roll_secs {
        return Err(TuliproxError::ConfigRecording(format!(
            "recording.default_post_roll_secs ({}) must not exceed max_post_roll_secs ({})",
            recording.default_post_roll_secs.unwrap_or(0),
            recording.max_post_roll_secs
        )));
    }

    // retention: each present policy must be > 0.
    if let Some(retention) = recording.retention.as_ref() {
        if let Some(keep) = retention.keep_last_per_channel {
            if keep == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.retention.keep_last_per_channel must be > 0".to_string(),
                ));
            }
        }
        if let Some(days) = retention.delete_after_days {
            if days == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.retention.delete_after_days must be > 0".to_string(),
                ));
            }
        }
        if retention.sweep_interval_secs == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.retention.sweep_interval_secs must be > 0".to_string(),
            ));
        }
        if retention.keep_last_per_channel.is_none() && retention.delete_after_days.is_none() {
            // A retention block that expresses no policy reads as
            // "retention is configured" while nothing is ever deleted.
            log::warn!(
                "recording.retention has neither keep_last_per_channel nor delete_after_days; \
                 no recording will ever be deleted by policy"
            );
        }
    }

    // notifications: an outbox that cannot hold or retry anything would
    // silently drop every lifecycle notification.
    if let Some(notifications) = recording.notifications.as_ref() {
        if notifications.outbox_buffer == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.notifications.outbox_buffer must be > 0".to_string(),
            ));
        }
        if notifications.max_attempts == 0 {
            return Err(TuliproxError::ConfigRecording("recording.notifications.max_attempts must be > 0".to_string()));
        }
        if notifications.backoff_initial_secs == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.notifications.backoff_initial_secs must be > 0".to_string(),
            ));
        }
        if notifications.backoff_max_secs < notifications.backoff_initial_secs {
            return Err(TuliproxError::ConfigRecording(format!(
                "recording.notifications.backoff_max_secs ({}) must be >= backoff_initial_secs ({})",
                notifications.backoff_max_secs, notifications.backoff_initial_secs
            )));
        }
    }

    // disk: percentages in 0..=100, low < high, cleanup > 0, safety non-zero.
    if let Some(disk) = recording.disk.as_ref() {
        if let Some(high) = disk.high_water_percent {
            if high > 100 {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.high_water_percent ({high}) must be 0..=100"
                )));
            }
        }
        if let Some(low) = disk.low_water_percent {
            if low > 100 {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.low_water_percent ({low}) must be 0..=100"
                )));
            }
        }
        if let (Some(low), Some(high)) = (disk.low_water_percent, disk.high_water_percent) {
            if low >= high {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.low_water_percent ({low}) must be < high_water_percent ({high})"
                )));
            }
        }
        if let Some(interval) = disk.cleanup_interval_secs {
            if interval == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.disk.cleanup_interval_secs must be > 0".to_string(),
                ));
            }
        }
        if let Some(safety) = disk.safety_bytes {
            if safety == 0 {
                return Err(TuliproxError::ConfigRecording("recording.disk.safety_bytes must be > 0".to_string()));
            }
        }
    }

    // quota: fallback bytes per minute must be > 0.
    if recording.fallback_bytes_per_minute == 0 {
        return Err(TuliproxError::ConfigRecording("recording.fallback_bytes_per_minute must be > 0".to_string()));
    }

    Ok(())
}
