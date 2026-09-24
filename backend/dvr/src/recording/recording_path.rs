//! One owner-independent layout for recording files.
//!
//! Two independent path computations used to exist: `RecordingTask::new` wrote
//! to `<root>[/<subdir>]/<filename>`, while the media endpoint resolved reads
//! through `resolve_recording_dir`, which invented a `users/<owner>/` or
//! `shared/` prefix that nothing ever wrote. Playback therefore looked for
//! every recording at a path where it did not exist.
//!
//! The layout is deliberately owner-independent. One physical file is shared
//! by every user who requested it, so keying its directory on an owner would
//! be wrong the moment a second user attaches, and would force the file to
//! move when the first detaches.

use shared::model::RecordingKind;
use std::path::{Component, Path, PathBuf};

/// Longest a single path component may be, in bytes.
///
/// 255 is the limit on ext4, APFS, NTFS and most others. The cap is applied on
/// a character boundary so a multi-byte title is truncated to something the
/// filesystem accepts rather than to invalid UTF-8.
const MAX_COMPONENT_BYTES: usize = 255;

/// Names Windows refuses regardless of extension.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Substituted for a component that sanitises down to nothing.
const PLACEHOLDER: &str = "untitled";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingPathError {
    /// The filename was empty, or sanitised down to nothing.
    EmptyFilename,
    /// The built path escaped, or could not be proven to stay under, the root.
    NotWithinRoot,
}

impl std::fmt::Display for RecordingPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyFilename => f.write_str("recording filename is empty"),
            Self::NotWithinRoot => f.write_str("recording path escapes the recording root"),
        }
    }
}

impl std::error::Error for RecordingPathError {}

/// What the layout groups a recording under, when directory organisation is
/// enabled. Resolved by the server from the catalog item; never client
/// supplied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecordingGrouping {
    /// Live: the channel the capture came from.
    pub channel: Option<String>,
    /// VOD: the title.
    pub title: Option<String>,
    /// Series: the series name and season number.
    pub series: Option<String>,
    pub season: Option<u32>,
}

impl RecordingGrouping {
    pub fn live(channel: impl Into<String>) -> Self { Self { channel: Some(channel.into()), ..Self::default() } }

    pub fn vod(title: impl Into<String>) -> Self { Self { title: Some(title.into()), ..Self::default() } }

    pub fn series(series: impl Into<String>, season: Option<u32>) -> Self {
        Self { series: Some(series.into()), season, ..Self::default() }
    }
}

/// Replaces anything that is not safe in a single path component.
fn sanitize_component(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            // Separators, the NUL byte, control characters and the characters
            // Windows forbids all collapse to `_`.
            if c == '/'
                || c == '\\'
                || c == '\0'
                || c.is_control()
                || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    // A leading dot would hide the file; a trailing dot or space is silently
    // stripped by Windows, which would make two distinct names collide.
    let trimmed = replaced.trim().trim_matches('.').trim();
    let capped = cap_at_char_boundary(trimmed, MAX_COMPONENT_BYTES);
    let capped = capped.trim().trim_end_matches('.').trim();
    if capped.is_empty() || is_windows_reserved(capped) {
        return PLACEHOLDER.to_owned();
    }
    capped.to_owned()
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    WINDOWS_RESERVED.iter().any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Truncates to at most `max_bytes`, never splitting a character.
fn cap_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.get(..end).unwrap_or("")
}

/// Sanitises a filename while preserving its extension.
fn sanitize_filename(raw: &str) -> Result<String, RecordingPathError> {
    let path = Path::new(raw);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if raw.trim().is_empty() {
        return Err(RecordingPathError::EmptyFilename);
    }
    let extension = sanitize_extension(extension);
    // The extension has to survive the length cap, so reserve room for it.
    let reserved = if extension.is_empty() { 0 } else { extension.len().saturating_add(1) };
    let stem_budget = MAX_COMPONENT_BYTES.saturating_sub(reserved).max(1);
    let stem = sanitize_component(cap_at_char_boundary(stem, stem_budget));
    Ok(if extension.is_empty() { stem } else { format!("{stem}.{extension}") })
}

fn sanitize_extension(extension: &str) -> String {
    extension.chars().filter(char::is_ascii_alphanumeric).take(16).collect()
}

/// Builds the relative path a recording is stored at, below the recording
/// root. Pure: it touches no filesystem and depends on no principal.
///
/// Organised layouts are `<channel>/<file>` for Live, `<title>/<file>` for
/// VOD and `<series>/Season NN/<file>` for a series episode. Unorganised is
/// the bare `<file>`.
pub fn build_relative_path(
    kind: RecordingKind,
    organize_into_directories: bool,
    grouping: &RecordingGrouping,
    filename: &str,
) -> Result<PathBuf, RecordingPathError> {
    let filename = sanitize_filename(filename)?;
    if !organize_into_directories {
        return Ok(PathBuf::from(filename));
    }
    let mut path = PathBuf::new();
    match kind {
        RecordingKind::Live => {
            if let Some(channel) = grouping.channel.as_deref().filter(|c| !c.trim().is_empty()) {
                path.push(sanitize_component(channel));
            }
        }
        RecordingKind::Vod => {
            if let Some(title) = grouping.title.as_deref().filter(|t| !t.trim().is_empty()) {
                path.push(sanitize_component(title));
            }
        }
        RecordingKind::Series => {
            if let Some(series) = grouping.series.as_deref().filter(|s| !s.trim().is_empty()) {
                path.push(sanitize_component(series));
                if let Some(season) = grouping.season {
                    path.push(format!("Season {season:02}"));
                }
            }
        }
    }
    path.push(filename);
    Ok(path)
}

/// Appends a collision suffix to the file component of `relative`, keeping
/// the directory part and the extension intact.
pub fn with_collision_suffix(relative: &Path, index: usize) -> PathBuf {
    let parent = relative.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = relative.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let extension = relative.extension().and_then(|s| s.to_str()).unwrap_or("");
    let name = if extension.is_empty() { format!("{stem}_{index}") } else { format!("{stem}_{index}.{extension}") };
    parent.join(name)
}

/// `true` when `relative` is a safe, non-escaping relative path.
pub fn is_contained_relative_path(relative: &Path) -> bool {
    let raw = relative.as_os_str();
    if raw.is_empty() || raw.as_encoded_bytes().contains(&0) || relative.is_absolute() {
        return false;
    }
    let mut saw_component = false;
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                if part.is_empty() {
                    return false;
                }
                saw_component = true;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) | Component::RootDir => return false,
        }
    }
    saw_component
}

/// Joins `relative` onto `root`, refusing anything that is not provably
/// contained. Purely lexical: the caller still has to open the result with
/// the no-follow helpers, because a component can become a symlink between
/// this check and the open.
pub fn resolve_under_root(root: &Path, relative: &Path) -> Result<PathBuf, RecordingPathError> {
    if !is_contained_relative_path(relative) {
        return Err(RecordingPathError::NotWithinRoot);
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::{
        build_relative_path, is_contained_relative_path, resolve_under_root, sanitize_component, with_collision_suffix,
        RecordingGrouping, RecordingPathError, MAX_COMPONENT_BYTES,
    };
    use shared::model::RecordingKind;
    use std::path::{Path, PathBuf};

    fn built(kind: RecordingKind, organize: bool, grouping: &RecordingGrouping, filename: &str) -> String {
        build_relative_path(kind, organize, grouping, filename)
            .expect("path builds")
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn the_layout_table_is_exact() {
        let cases: &[(RecordingKind, bool, RecordingGrouping, &str, &str)] = &[
            (RecordingKind::Live, true, RecordingGrouping::live("BBC One"), "news.ts", "BBC One/news.ts"),
            (RecordingKind::Vod, true, RecordingGrouping::vod("The Film"), "the_film.mp4", "The Film/the_film.mp4"),
            (
                RecordingKind::Series,
                true,
                RecordingGrouping::series("The Show", Some(1)),
                "s01e02.mkv",
                "The Show/Season 01/s01e02.mkv",
            ),
            (
                RecordingKind::Series,
                true,
                RecordingGrouping::series("The Show", Some(12)),
                "s12e02.mkv",
                "The Show/Season 12/s12e02.mkv",
            ),
            // Unorganised: the bare file, directly under the recording root.
            (RecordingKind::Live, false, RecordingGrouping::live("BBC One"), "news.ts", "news.ts"),
            (RecordingKind::Vod, false, RecordingGrouping::vod("The Film"), "the_film.mp4", "the_film.mp4"),
            // A missing grouping degrades to the flat layout rather than
            // inventing a directory name.
            (RecordingKind::Vod, true, RecordingGrouping::default(), "orphan.mp4", "orphan.mp4"),
            (RecordingKind::Series, true, RecordingGrouping::series("The Show", None), "e1.mkv", "The Show/e1.mkv"),
        ];
        for (kind, organize, grouping, filename, expected) in cases {
            assert_eq!(&built(*kind, *organize, grouping, filename), expected, "{kind} organize={organize}");
        }
    }

    #[test]
    fn no_layout_carries_an_owner_or_visibility_component() {
        // One physical file is shared by every user that asked for it, so the
        // path must not depend on who asked first.
        for kind in [RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series] {
            for organize in [true, false] {
                let grouping = RecordingGrouping {
                    channel: Some("Chan".into()),
                    title: Some("Title".into()),
                    series: Some("Series".into()),
                    season: Some(3),
                };
                let path = built(kind, organize, &grouping, "file.ts");
                for forbidden in ["users/", "shared/", "private/"] {
                    assert!(!path.contains(forbidden), "{path} carries {forbidden}");
                }
            }
        }
    }

    #[test]
    fn separators_and_traversal_cannot_escape_a_component() {
        let grouping = RecordingGrouping::vod("../../etc");
        let path = built(RecordingKind::Vod, true, &grouping, "passwd");
        // Separators collapse first, then the leading dot is stripped so the
        // directory is not hidden. Either way it is one flat component.
        assert_eq!(path, "_.._etc/passwd");
        assert!(is_contained_relative_path(Path::new(&path)));

        let nested = built(RecordingKind::Vod, true, &RecordingGrouping::vod("a/b"), "c/d.ts");
        assert_eq!(nested, "a_b/d.ts", "a separator in a title must not create a directory");
    }

    #[test]
    fn an_absolute_filename_is_reduced_to_its_last_component() {
        assert_eq!(built(RecordingKind::Vod, false, &RecordingGrouping::default(), "/etc/passwd"), "passwd");
    }

    #[test]
    fn unsafe_and_control_characters_are_replaced() {
        assert_eq!(sanitize_component("a:b*c?d\"e<f>g|h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_component("tab\there"), "tab_here");
        assert_eq!(sanitize_component("nul\0byte"), "nul_byte");
    }

    #[test]
    fn empty_and_dot_components_fall_back_to_a_placeholder() {
        for raw in ["", "   ", ".", "..", "...", " . "] {
            assert_eq!(sanitize_component(raw), "untitled", "{raw:?}");
        }
    }

    #[test]
    fn windows_reserved_names_are_replaced() {
        for raw in ["CON", "con", "NUL", "com1", "LPT9", "AUX.ts"] {
            assert_eq!(sanitize_component(raw), "untitled", "{raw}");
        }
        // A name that merely starts with a reserved word is fine.
        assert_eq!(sanitize_component("CONCERT"), "CONCERT");
    }

    #[test]
    fn a_component_is_capped_on_a_character_boundary() {
        // Four-byte characters: a naive byte truncation would split one and
        // produce invalid UTF-8.
        let long = "🎬".repeat(200);
        let capped = sanitize_component(&long);
        assert!(capped.len() <= MAX_COMPONENT_BYTES, "{} bytes", capped.len());
        assert!(capped.chars().all(|c| c == '🎬'));
        assert_eq!(capped.len() % 4, 0, "a character was split");
    }

    #[test]
    fn a_long_filename_keeps_its_extension() {
        let filename = format!("{}.mkv", "n".repeat(400));
        let built = built(RecordingKind::Vod, false, &RecordingGrouping::default(), &filename);
        assert_eq!(Path::new(&built).extension().and_then(|e| e.to_str()), Some("mkv"), "{built}");
        assert!(built.len() <= MAX_COMPONENT_BYTES, "{} bytes", built.len());
    }

    #[test]
    fn an_empty_filename_is_refused() {
        for raw in ["", "   "] {
            assert_eq!(
                build_relative_path(RecordingKind::Vod, false, &RecordingGrouping::default(), raw),
                Err(RecordingPathError::EmptyFilename)
            );
        }
    }

    #[test]
    fn a_collision_suffix_keeps_the_directory_and_extension() {
        assert_eq!(
            with_collision_suffix(Path::new("Show/Season 01/e1.mkv"), 2),
            PathBuf::from("Show/Season 01/e1_2.mkv")
        );
        assert_eq!(with_collision_suffix(Path::new("e1"), 3), PathBuf::from("e1_3"));
    }

    #[test]
    fn containment_rejects_escapes() {
        for bad in ["", "..", "../x", "a/../../b", "/abs/path", "./x"] {
            assert!(!is_contained_relative_path(Path::new(bad)), "{bad} must be refused");
        }
        for good in ["a", "a/b", "Show/Season 01/e1.mkv"] {
            assert!(is_contained_relative_path(Path::new(good)), "{good} must be accepted");
        }
    }

    #[test]
    fn resolve_under_root_refuses_to_leave_the_root() {
        let root = Path::new("/recordings");
        assert_eq!(resolve_under_root(root, Path::new("a/b.ts")).expect("joins"), PathBuf::from("/recordings/a/b.ts"));
        for bad in ["../escape.ts", "/etc/passwd", ".."] {
            assert_eq!(resolve_under_root(root, Path::new(bad)), Err(RecordingPathError::NotWithinRoot));
        }
    }

    #[test]
    fn the_written_path_is_the_path_a_reader_resolves() {
        // Regression: the writer stored `<root>[/<subdir>]/<file>` while the
        // media endpoint resolved `<root>/users/<owner>/<file>`, so playback
        // looked for every recording where it had never been written.
        let root = Path::new("/recordings");
        let relative = build_relative_path(
            RecordingKind::Series,
            true,
            &RecordingGrouping::series("The Show", Some(2)),
            "e03.mkv",
        )
        .expect("path builds");

        let written = root.join(&relative);
        let read_back = resolve_under_root(root, &relative).expect("resolves");
        assert_eq!(written, read_back);
        assert_eq!(read_back, PathBuf::from("/recordings/The Show/Season 02/e03.mkv"));
    }

    #[test]
    fn every_built_path_is_contained() {
        // Whatever the inputs, the builder must never produce something the
        // containment check would refuse.
        let nasty = ["../..", "/", "\\", "C:", "...", "\0", "  ", "CON", "a/b/c"];
        for kind in [RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series] {
            for group in nasty {
                let grouping = RecordingGrouping {
                    channel: Some(group.into()),
                    title: Some(group.into()),
                    series: Some(group.into()),
                    season: Some(1),
                };
                for filename in ["ok.ts", "../../escape.ts", "/abs.ts"] {
                    let Ok(path) = build_relative_path(kind, true, &grouping, filename) else { continue };
                    assert!(is_contained_relative_path(&path), "{kind} {group:?} {filename:?} produced {path:?}");
                }
            }
        }
    }
}
