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
/// where a valid one used to be. Rewriting an existing sidecar is normal: a
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
}
