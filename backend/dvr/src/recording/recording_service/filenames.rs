use super::{deletion::collect_existing_relative_paths, CreateRecordingInput, EffectiveRecordingWindow};
use crate::recording::{
    recording_path,
    recording_queue::{PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError},
};
use std::path::{Path, PathBuf};

/// Maximum bytes in a single sanitized filename component. Well under
/// the 255-byte limit every supported filesystem enforces, leaving room
/// for the `_N` disambiguation suffix and the `.partial` extension the
/// worker appends.
pub(super) const MAX_FILENAME_COMPONENT_BYTES: usize = 200;

/// Substitute for a title that sanitizes down to nothing.
const FILENAME_FALLBACK: &str = "recording";

/// Characters that are illegal in a path component on at least one
/// supported platform. `/` and `\` are separators where it matters; the
/// rest are Windows-reserved but are equally unwelcome in a
/// URL-addressed media path.
const FILENAME_FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windows reserved device names. A component whose stem matches one of
/// these (case-insensitively) cannot be created on Windows, with or
/// without an extension.
const WINDOWS_RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Turn arbitrary programme text into one safe path component.
///
/// The previous implementation replaced only the two path separators,
/// which let control characters, Windows-reserved characters, trailing
/// dots/spaces, and `BiDi` override codepoints through into the path the
/// muxer opens and the media API re-validates. This is the single
/// chokepoint: everything that lands in `filename` goes through here.
///
/// Guarantees on the returned string:
/// - exactly one path component (no separator survives),
/// - no ASCII control characters and no Unicode `BiDi` / invisible
///   formatting codepoints,
/// - no leading or trailing whitespace or `.`,
/// - never empty, never `.` or `..`, never a Windows device name,
/// - at most `MAX_FILENAME_COMPONENT_BYTES` bytes, truncated on a
///   character boundary,
/// - idempotent: sanitizing an already-sanitized value is a no-op.
pub fn sanitize_filename_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_underscore = false;
    for ch in raw.chars() {
        // Invisible formatting codepoints can reorder the rendered
        // filename so it does not match the bytes on disk. Drop them
        // outright rather than substituting, so they leave no trace.
        if is_invisible_formatting(ch) {
            continue;
        }
        if ch.is_control() || FILENAME_FORBIDDEN_CHARS.contains(&ch) {
            // Collapse runs so `a///b` becomes `a_b`, not `a___b`.
            if !last_was_underscore {
                out.push('_');
                last_was_underscore = true;
            }
            continue;
        }
        out.push(ch);
        last_was_underscore = ch == '_';
    }

    // Trailing dots and spaces are silently stripped by Windows, which
    // would desync the persisted `relative_path` from the real file.
    let trimmed = out.trim_matches(|ch: char| ch.is_whitespace() || ch == '.');
    let mut result = truncate_on_char_boundary(trimmed, MAX_FILENAME_COMPONENT_BYTES)
        .trim_end_matches(|ch: char| ch.is_whitespace() || ch == '.')
        .to_string();

    if result.is_empty() || is_windows_reserved_stem(&result) {
        result = FILENAME_FALLBACK.to_string();
    }
    result
}

/// `BiDi` controls, zero-width characters, and the other invisible
/// formatting codepoints that make a filename render differently from
/// what it actually contains.
fn is_invisible_formatting(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'      // zero-width space .. RLM
            | '\u{202a}'..='\u{202e}' // embedding / override
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // directional isolates
            | '\u{feff}'              // BOM / zero-width no-break space
    )
}

/// Truncate to at most `max_bytes`, never splitting a character.
fn truncate_on_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// `true` when the component's stem is a Windows device name.
fn is_windows_reserved_stem(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    WINDOWS_RESERVED_STEMS.iter().any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Live recording filename: `recording.filename_template` rendered for this
/// programme, plus the extension of the container the worker muxes into, so
/// players and the media API can tell the format from the name.
///
/// The sanitized programme title stands in when the template is unset or
/// cannot be rendered.
pub(super) fn render_live_filename(
    input: &CreateRecordingInput,
    recording_cfg: &tuliprox_core::model::RecordingConfig,
    window: &EffectiveRecordingWindow,
    owner_display: &str,
) -> String {
    let title_stem = sanitize_filename_component(&input.program_title);
    let context = shared::utils::RecordingFilenameContext {
        task_id: title_stem.clone(),
        channel_id: input.channel_id.clone(),
        channel_name: input.channel_name.clone(),
        program_title: Some(input.program_title.clone()),
        episode_season: input.epg.as_ref().and_then(|epg| epg.season),
        episode_number: input.epg.as_ref().and_then(|epg| epg.episode),
        owner_display: (!owner_display.trim().is_empty()).then(|| owner_display.to_string()),
        program_start: Some(input.program_start),
        program_end: Some(input.program_end),
        scheduled_start: Some(window.scheduled_start),
        scheduled_end: Some(window.scheduled_end),
    };
    let stem = Some(recording_cfg.filename_template.as_str())
        .filter(|template| !template.trim().is_empty())
        .and_then(|template| {
            shared::utils::render_recording_stem(template, &context, &recording_cfg.timezone)
                .map_err(|err| log::warn!("Recording filename template could not be rendered: {err}"))
                .ok()
        })
        .unwrap_or(title_stem);
    let extension = recording_cfg.container_format.file_extension();
    // A template that already ends in the container extension must not double it.
    let stem_path = Path::new(&stem);
    let stem = match (stem_path.file_stem(), stem_path.extension()) {
        (Some(base), Some(ext)) if ext.eq_ignore_ascii_case(extension) => base.to_string_lossy(),
        _ => std::borrow::Cow::Borrowed(stem.as_str()),
    };
    format!("{stem}.{extension}")
}

pub(super) fn reserve_recording_relative_path(
    candidate: &PersistedRecordingQueue,
    task: &mut PersistedRecordingTask,
) -> Result<(), QueueMutationError> {
    // Borrowed set, built once. The old code walked a `Vec<String>` of
    // cloned filenames once per `_N` candidate, so reserving the
    // (N+1)-th recording of a title cost O(N^2) string comparisons.
    let existing: std::collections::HashSet<&str> = collect_existing_relative_paths(candidate).collect();
    // The reservation is over the whole root-relative path, not the bare
    // filename: two series can legitimately hold an `e01.mkv` in different
    // season directories.
    let base = task.recording.relative_path.clone().unwrap_or_else(|| task.filename.clone());
    let mut relative = PathBuf::from(&base);
    if existing.contains(base.as_str()) {
        // Linear probe over indices; each probe is one hash lookup.
        for index in 1.. {
            relative = recording_path::with_collision_suffix(Path::new(&base), index);
            if !existing.contains(relative.to_string_lossy().as_ref()) {
                break;
            }
        }
    }
    let filename =
        relative.file_name().and_then(|name| name.to_str()).ok_or(QueueMutationError::InvalidPath)?.to_string();
    validate_reserved_filename(&filename).map_err(|_| QueueMutationError::InvalidPath)?;
    if !recording_path::is_contained_relative_path(&relative) {
        return Err(QueueMutationError::InvalidPath);
    }
    task.file_path = task.file_dir.join(&filename);
    task.filename = filename;
    task.recording.relative_path = Some(relative.to_string_lossy().into_owned());
    Ok(())
}

pub(super) fn validate_reserved_filename(filename: &str) -> Result<(), &'static str> {
    use std::path::Component;
    let path = Path::new(filename);
    let single_normal_component =
        path.components().next().is_some_and(|c| matches!(c, Component::Normal(_))) && path.components().count() == 1;
    if filename.is_empty() || path.is_absolute() || !single_normal_component || filename.as_bytes().contains(&0) {
        return Err("recording invalid path");
    }
    Ok(())
}
