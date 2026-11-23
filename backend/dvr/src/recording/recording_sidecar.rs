//! The physical record written beside a finished recording.
//!
//! A sidecar describes the *file*, never the people who asked for it. It exists
//! so an operator, or a rebuild, can tell what an orphaned recording on disk
//! actually is. It is not an access grant: rights live only in the repository,
//! and a sidecar found on disk can rediscover a file but never re-create the
//! library entry that would let somebody play it.

use serde::{Deserialize, Serialize};
use shared::model::RecordingKind;
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

/// Suffix appended to the recording's own filename.
///
/// The organised layouts put many recordings in one directory, so a single
/// fixed name per directory would describe only whichever finished last.
pub const SIDECAR_SUFFIX: &str = ".tuliprox-recording.json";

/// What a finished recording is, as told by the file next to it.
///
/// Every field here is a physical fact. Owner, visibility, quota, headers, URL
/// and resume validators are deliberately absent: this file sits in the
/// recording directory, which is not a place to put anything user-specific or
/// secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSidecar {
    pub materialization_id: String,
    pub media_identity: String,
    pub kind: RecordingKind,
    pub relative_path: String,
    pub size_bytes: u64,
    pub completed_at: i64,
}

/// The sidecar that belongs to a recording file.
pub fn sidecar_path(recording: &Path) -> PathBuf {
    let mut name = recording.as_os_str().to_os_string();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// `true` when this path is a sidecar rather than a recording.
pub fn is_sidecar(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(SIDECAR_SUFFIX))
}

/// Write the sidecar beside its recording, replacing any earlier one.
///
/// Staged and renamed so a crash mid-write cannot leave a half-parsed file
/// where a valid one was. Rewriting an existing sidecar is normal: a
/// finalization that runs twice must not fail the second time.
pub async fn write_sidecar(recording: &Path, sidecar: &RecordingSidecar) -> io::Result<PathBuf> {
    let target = sidecar_path(recording);
    let staging = sidecar_path(recording).with_extension("writing");
    let encoded = serde_json::to_vec_pretty(sidecar).map_err(io::Error::other)?;

    let mut file = tokio::fs::File::create(&staging).await?;
    file.write_all(&encoded).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&staging, &target).await?;
    Ok(target)
}

/// Read one sidecar. A file that does not parse is not a sidecar.
pub async fn read_sidecar(path: &Path) -> io::Result<RecordingSidecar> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Suffix appended to the recording's own filename for its time index.
pub const TIME_INDEX_SUFFIX: &str = ".tuliprox-recording.idx";

/// One sample in the time index.
/// Maximum allowable gap in seconds between consecutive time index entries
/// within a clip's window before it is considered a gap in the recording.
pub const MAX_INDEX_SAMPLE_GAP_SECS: i64 = 15;

/// Check whether the time index entries covering `[start_ts, end_ts]` contain a gap.
/// A gap occurs if:
/// 1. The index is empty.
/// 2. The time index does not begin early enough to cover `start_ts` within `MAX_INDEX_SAMPLE_GAP_SECS`.
/// 3. The time index does not reach `end_ts` within `MAX_INDEX_SAMPLE_GAP_SECS`.
/// 4. File size decreases between consecutive samples in the clip window (restart/truncation).
/// 5. The timestamp difference between consecutive samples within the window exceeds `MAX_INDEX_SAMPLE_GAP_SECS`.
pub fn has_clip_time_gap(index: &[TimeIndexEntry], start_ts: i64, end_ts: i64) -> bool {
    if index.is_empty() {
        return true;
    }
    if index[0].unix_secs > start_ts + MAX_INDEX_SAMPLE_GAP_SECS {
        return true;
    }
    if index[index.len() - 1].unix_secs + MAX_INDEX_SAMPLE_GAP_SECS < end_ts {
        return true;
    }
    let start_idx = index.iter().rposition(|e| e.unix_secs <= start_ts).unwrap_or(0);
    let end_idx = index.iter().position(|e| e.unix_secs >= end_ts).unwrap_or(index.len().saturating_sub(1));

    let slice = &index[start_idx..=end_idx.min(index.len().saturating_sub(1))];
    for window in slice.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        if curr.file_size < prev.file_size {
            return true;
        }
        if curr.unix_secs.saturating_sub(prev.unix_secs) > MAX_INDEX_SAMPLE_GAP_SECS {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeIndexEntry {
    pub unix_secs: i64,
    pub file_size: u64,
}

/// The time index file that belongs to a recording file.
pub fn time_index_path(recording: &Path) -> PathBuf {
    let mut name = recording.as_os_str().to_os_string();
    name.push(TIME_INDEX_SUFFIX);
    PathBuf::from(name)
}

/// `true` when this path is a time index file.
pub fn is_time_index(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(TIME_INDEX_SUFFIX))
}

/// Append one `(unix_secs, file_size)` entry to the recording's index file.
///
/// Written in an append-only, line-based format (`{unix_secs} {file_size}\n`).
pub async fn append_time_index_entry(index_path: &Path, unix_secs: i64, file_size: u64) -> io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(index_path).await?;
    let line = format!("{unix_secs} {file_size}\n");
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

/// Read the time index entries. A truncated or half-written last line is skipped.
pub async fn read_time_index(index_path: &Path) -> io::Result<Vec<TimeIndexEntry>> {
    let path = index_path.to_path_buf();
    tokio::task::spawn_blocking(move || read_time_index_sync(&path)).await.map_err(io::Error::other)?
}

/// Synchronous reader for the time index entries. A truncated or half-written last line is skipped.
pub fn read_time_index_sync(index_path: &Path) -> io::Result<Vec<TimeIndexEntry>> {
    let content = match std::fs::read_to_string(index_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(parse_time_index_content(&content))
}

fn parse_time_index_content(content: &str) -> Vec<TimeIndexEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(secs_str), Some(size_str)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(unix_secs), Ok(file_size)) = (secs_str.parse::<i64>(), size_str.parse::<u64>()) else {
            continue;
        };
        entries.push(TimeIndexEntry { unix_secs, file_size });
    }
    entries
}

/// Every sidecar under `root` whose materialization is not already known.
///
/// Returns physical descriptions only. Nothing here creates a library entry or
/// grants anyone access -- that is the point of the return type: an orphan is
/// something for an operator to look at, not something to hand to a user.
/// Unreadable or unparseable files are skipped rather than failing the scan;
/// one bad file must not hide every good one.
pub async fn scan_orphans(root: &Path, known_materializations: &[String]) -> io::Result<Vec<RecordingSidecar>> {
    let mut found = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // `file_type` does not follow symlinks, so a link cannot walk the
            // scan out of the recording root.
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file() && is_sidecar(&path) {
                if let Ok(sidecar) = read_sidecar(&path).await {
                    if !known_materializations.iter().any(|known| known == &sidecar.materialization_id) {
                        found.push(sidecar);
                    }
                }
            }
        }
    }
    found.sort_by(|left, right| left.materialization_id.cmp(&right.materialization_id));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::{is_sidecar, read_sidecar, scan_orphans, sidecar_path, write_sidecar, RecordingSidecar};
    use shared::model::RecordingKind;
    use std::path::Path;
    use tempfile::TempDir;

    fn sidecar(id: &str) -> RecordingSidecar {
        RecordingSidecar {
            materialization_id: id.to_string(),
            media_identity: format!("programme-{id}"),
            kind: RecordingKind::Vod,
            relative_path: format!("{id}.mp4"),
            size_bytes: 4_096,
            completed_at: 1_700_000_000,
        }
    }

    #[test]
    fn a_sidecar_is_named_after_its_recording() {
        // A fixed name per directory would describe only the last recording to
        // finish, and the organised layouts share directories.
        let first = sidecar_path(Path::new("/rec/Channel/one.ts"));
        let second = sidecar_path(Path::new("/rec/Channel/two.ts"));
        assert_ne!(first, second, "two recordings in one directory need two sidecars");
        assert!(is_sidecar(&first));
        assert!(!is_sidecar(Path::new("/rec/Channel/one.ts")));
    }

    #[tokio::test]
    async fn a_sidecar_round_trips() {
        let dir = TempDir::new().expect("tempdir");
        let recording = dir.path().join("film.mp4");
        let written = write_sidecar(&recording, &sidecar("mat-a")).await.expect("write");
        assert_eq!(read_sidecar(&written).await.expect("read"), sidecar("mat-a"));
    }

    #[tokio::test]
    async fn writing_twice_replaces_rather_than_fails() {
        // Finalization is idempotent, so the sidecar write has to be too.
        let dir = TempDir::new().expect("tempdir");
        let recording = dir.path().join("film.mp4");
        write_sidecar(&recording, &sidecar("mat-a")).await.expect("first");
        let mut grown = sidecar("mat-a");
        grown.size_bytes = 8_192;
        let written = write_sidecar(&recording, &grown).await.expect("second");
        assert_eq!(read_sidecar(&written).await.expect("read").size_bytes, 8_192);
        assert!(!sidecar_path(&recording).with_extension("writing").exists(), "no staging file is left behind");
    }

    #[tokio::test]
    async fn a_sidecar_carries_no_user_or_transport_detail() {
        // It lives in the recording directory, which is not a place for owners,
        // credentials or provider URLs.
        let encoded = serde_json::to_string(&sidecar("mat-a")).expect("encode");
        for forbidden in ["owner", "user", "visibility", "quota", "header", "url", "etag", "token", "password"] {
            assert!(!encoded.to_lowercase().contains(forbidden), "{forbidden} must not appear in {encoded}");
        }
    }

    #[tokio::test]
    async fn an_unknown_sidecar_is_reported_as_an_orphan() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("Channel/Season 01");
        std::fs::create_dir_all(&nested).expect("dirs");
        write_sidecar(&dir.path().join("known.mp4"), &sidecar("mat-known")).await.expect("write");
        write_sidecar(&nested.join("lost.mp4"), &sidecar("mat-lost")).await.expect("write");

        let orphans = scan_orphans(dir.path(), &["mat-known".to_string()]).await.expect("scan");

        assert_eq!(orphans.len(), 1, "only the file the repository does not know about");
        assert_eq!(orphans[0].materialization_id, "mat-lost");
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_sidecar_is_ignored() {
        // Including one that is unreadable: a single bad file must not hide
        // every good one from the operator.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("film.mp4"), b"not json").expect("recording");
        std::fs::write(dir.path().join("broken.mp4.tuliprox-recording.json"), b"{ not json").expect("broken");
        write_sidecar(&dir.path().join("good.mp4"), &sidecar("mat-good")).await.expect("write");

        let orphans = scan_orphans(dir.path(), &[]).await.expect("scan");

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].materialization_id, "mat-good");
    }

    #[tokio::test]
    async fn rediscovering_an_orphan_describes_the_file_and_nothing_more() {
        // The whole risk of scanning the recording directory is that a file
        // found there becomes a way in. A sidecar carries no principal, so
        // there is nothing here to build a library entry from: an orphan can be
        // identified and no more. Access comes from the repository or not at
        // all.
        let dir = TempDir::new().expect("tempdir");
        write_sidecar(&dir.path().join("lost.mp4"), &sidecar("mat-lost")).await.expect("write");

        let orphans = scan_orphans(dir.path(), &[]).await.expect("scan");

        let found = &orphans[0];
        let encoded = serde_json::to_value(found).expect("encode");
        let fields: Vec<&str> = encoded.as_object().expect("object").keys().map(String::as_str).collect();
        assert_eq!(
            fields,
            ["materialization_id", "media_identity", "kind", "relative_path", "size_bytes", "completed_at"],
            "a sidecar describes a file; anything more would be a claim about who may have it"
        );
    }

    #[tokio::test]
    async fn scanning_a_missing_root_is_not_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let orphans = scan_orphans(&dir.path().join("nothing-here"), &[]).await.expect("scan");
        assert!(orphans.is_empty(), "an absent recording directory just has no orphans");
    }

    #[tokio::test]
    async fn time_index_appends_and_skips_truncated_line() {
        use super::{append_time_index_entry, is_time_index, read_time_index, time_index_path, TimeIndexEntry};
        use tokio::io::AsyncWriteExt;

        let dir = TempDir::new().expect("tempdir");
        let recording = dir.path().join("live.ts");
        let index_path = time_index_path(&recording);
        assert!(is_time_index(&index_path));
        assert!(!is_time_index(&recording));

        append_time_index_entry(&index_path, 1_700_000_001, 10_000).await.expect("append 1");
        append_time_index_entry(&index_path, 1_700_000_002, 25_000).await.expect("append 2");

        // Write a truncated partial line at the end
        let mut file = tokio::fs::OpenOptions::new().append(true).open(&index_path).await.expect("open");
        file.write_all(b"1700000003 partial_gar").await.expect("write partial");
        drop(file);

        let entries = read_time_index(&index_path).await.expect("read");
        assert_eq!(
            entries,
            vec![
                TimeIndexEntry { unix_secs: 1_700_000_001, file_size: 10_000 },
                TimeIndexEntry { unix_secs: 1_700_000_002, file_size: 25_000 },
            ]
        );
    }

    #[test]
    fn has_clip_time_gap_matrix_tests() {
        use super::{has_clip_time_gap, TimeIndexEntry, MAX_INDEX_SAMPLE_GAP_SECS};

        // 1. Empty index is a gap
        assert!(has_clip_time_gap(&[], 1000, 2000));

        let complete = vec![
            TimeIndexEntry { unix_secs: 1000, file_size: 100 },
            TimeIndexEntry { unix_secs: 1010, file_size: 200 },
            TimeIndexEntry { unix_secs: 1020, file_size: 300 },
        ];
        // Valid coverage
        assert!(!has_clip_time_gap(&complete, 1005, 1015));
        assert!(!has_clip_time_gap(&complete, 1000, 1020));

        // 2. Start gap: first sample too late (> start_ts + MAX_INDEX_SAMPLE_GAP_SECS)
        assert!(has_clip_time_gap(&complete, 1000 - MAX_INDEX_SAMPLE_GAP_SECS - 1, 1020));
        assert!(!has_clip_time_gap(&complete, 1000 - MAX_INDEX_SAMPLE_GAP_SECS, 1020));

        // 3. End gap: last sample ends before end_ts (last + MAX_GAP < end_ts)
        assert!(has_clip_time_gap(&complete, 1000, 1020 + MAX_INDEX_SAMPLE_GAP_SECS + 1));
        assert!(!has_clip_time_gap(&complete, 1000, 1020 + MAX_INDEX_SAMPLE_GAP_SECS));

        // 4. Intermediate gap: delta between consecutive samples exceeds MAX_INDEX_SAMPLE_GAP_SECS
        let with_gap = vec![
            TimeIndexEntry { unix_secs: 1000, file_size: 100 },
            TimeIndexEntry { unix_secs: 1000 + MAX_INDEX_SAMPLE_GAP_SECS + 1, file_size: 200 },
            TimeIndexEntry { unix_secs: 1050, file_size: 300 },
        ];
        assert!(has_clip_time_gap(&with_gap, 1000, 1050));

        // 5. File size decreases: restart / truncation
        let with_decrease = vec![
            TimeIndexEntry { unix_secs: 1000, file_size: 200 },
            TimeIndexEntry { unix_secs: 1005, file_size: 150 },
            TimeIndexEntry { unix_secs: 1010, file_size: 300 },
        ];
        assert!(has_clip_time_gap(&with_decrease, 1000, 1010));
    }
}
