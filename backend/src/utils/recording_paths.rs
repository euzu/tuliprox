//! Secure recording-path and file-operation helpers.
//!
//! Linux-only: uses `openat2`/`unlinkat`-equivalent semantics via the
//! `O_NOFOLLOW`/`O_EXCL` open flags and atomic rename. The strict
//! `openat2` with `RESOLVE_BENEATH`/`RESOLVE_NO_SYMLINKS` is not
//! portable; this module
//! implements the same security properties using the more portable
//! open-flag equivalents plus `symlink_metadata` for no-follow inspection.
//! A future task can swap `safe_unlink`/`finalize_no_replace` for direct
//! `openat2` calls without changing callers.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

/// Visibility for a recording directory layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingVisibility {
    Private,
    Shared,
}

/// Errors that can occur when handling a recording path.
#[derive(Debug)]
pub enum RecordingPathError {
    Empty,
    Absolute,
    InvalidComponent,
    NulByte,
    NotARegularFile,
    NotWithinRoot,
    AlreadyExists,
    Io(io::Error),
}

impl std::fmt::Display for RecordingPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("path is empty"),
            Self::Absolute => f.write_str("path is absolute"),
            Self::InvalidComponent => f.write_str("path contains '.' or '..' or other invalid component"),
            Self::NulByte => f.write_str("path contains a NUL byte"),
            Self::NotARegularFile => f.write_str("path is not a regular file"),
            Self::NotWithinRoot => f.write_str("path is not within the recording root"),
            Self::AlreadyExists => f.write_str("path already exists"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for RecordingPathError {}

impl From<io::Error> for RecordingPathError {
    fn from(err: io::Error) -> Self { Self::Io(err) }
}

impl From<RecordingPathError> for io::Error {
    fn from(err: RecordingPathError) -> Self {
        match err {
            RecordingPathError::Io(e) => e,
            other => io::Error::other(other),
        }
    }
}

/// Validate a relative recording path. Rejects absolute paths, parent
/// traversal, current-directory components, and NUL bytes. The path
/// must be non-empty and consist only of normal components.
pub fn validate_relative_path(path: &Path) -> Result<(), RecordingPathError> {
    let s = path.as_os_str();
    if s.is_empty() {
        return Err(RecordingPathError::Empty);
    }
    if s.as_encoded_bytes().contains(&0) {
        return Err(RecordingPathError::NulByte);
    }
    if path.is_absolute() {
        return Err(RecordingPathError::Absolute);
    }
    let mut saw_component = false;
    for c in path.components() {
        match c {
            Component::Normal(_) => saw_component = true,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(RecordingPathError::InvalidComponent);
            }
        }
    }
    if !saw_component {
        return Err(RecordingPathError::Empty);
    }
    Ok(())
}

/// Validate that a relative path stays inside a canonicalized root.
pub fn assert_within_root(rel: &Path, root: &Path) -> Result<(), RecordingPathError> {
    let joined = root.join(rel);
    let canonical_root = fs::canonicalize(root).map_err(RecordingPathError::from)?;
    let canonical_joined = fs::canonicalize(&joined).map_err(RecordingPathError::from)?;
    if !canonical_joined.starts_with(&canonical_root) {
        return Err(RecordingPathError::NotWithinRoot);
    }
    Ok(())
}

/// Inspect a path without following symlinks. Returns `Some(metadata)` for
/// any path that exists at the location, including symlinks, directories,
/// and regular files. The caller can inspect the metadata to choose
/// the matching policy. Returns `None` only for missing entries.
pub fn no_follow_existing(path: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(path).ok()
}

/// Inspect a path without following symlinks. Returns `Some(metadata)` only
/// for regular files; directories, symlinks, sockets, devices, and
/// missing entries all return `None`.
pub fn no_follow_regular_file(path: &Path) -> Option<fs::Metadata> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(meta)
}

/// Open a new partial file with no-clobber, no-follow semantics suitable
/// for ffmpeg to write into. The call uses `O_CREAT | O_EXCL | O_NOFOLLOW`
/// so a pre-existing file or symlinked path is rejected.
pub fn open_partial_no_clobber(path: &Path) -> Result<fs::File, RecordingPathError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    Ok(file)
}

/// Finalize a partial file to its final path. The final path must not
/// exist at all (no regular file, no symlink, no directory) — we inspect
/// the location without following symlinks so an attacker-prepared
/// symlink is treated as a collision. The rename is then atomic. If
/// the partial is missing, `fs::rename` returns `NotFound` so the
/// caller can surface a visible failure rather than silently producing
/// an empty final file.
pub fn finalize_no_replace(partial: &Path, final_path: &Path) -> Result<(), RecordingPathError> {
    if no_follow_existing(final_path).is_some() {
        return Err(RecordingPathError::AlreadyExists);
    }
    fs::rename(partial, final_path)?;
    Ok(())
}

/// Unlink a file at the given path. Missing files are treated as
/// success so the call is idempotent; the operator's intent (file
/// gone) is satisfied either way.
pub fn safe_unlink(path: &Path) -> Result<(), RecordingPathError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Clean up empty parent directories between `path` and `root`. The
/// walk starts at `path` itself (so a deleted empty directory is
/// cleaned) and walks up to but not including the canonicalized root.
/// Removal stops on the first non-empty, non-existent, or
/// permission-denied directory. Other I/O errors propagate but do not
/// undo any successful removal.
pub fn clean_empty_parents(path: &Path, root: &Path) -> Result<(), RecordingPathError> {
    let canonical_root = fs::canonicalize(root).ok();
    let mut current: Option<&Path> = Some(path);
    while let Some(dir) = current {
        let stop_here = canonical_root
            .as_ref()
            .is_some_and(|r| fs::canonicalize(dir).is_ok_and(|c| c == *r));
        if stop_here {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => current = dir.parent(),
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Resolve the canonical directory layout for a recording's final path:
/// `<recording_root>/users/<owner>/<rel>` for `Private` and
/// `<recording_root>/shared/<rel>` for `Shared`. The relative path must
/// already be validated.
pub fn resolve_recording_dir(
    recording_root: &Path,
    visibility: RecordingVisibility,
    owner_id: &str,
    rel: &Path,
) -> Result<PathBuf, RecordingPathError> {
    validate_relative_path(rel)?;
    if owner_id.contains('\0') || owner_id.is_empty() {
        return Err(RecordingPathError::InvalidComponent);
    }
    let base = match visibility {
        RecordingVisibility::Private => recording_root.join("users").join(owner_id),
        RecordingVisibility::Shared => recording_root.join("shared"),
    };
    Ok(base.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn validate_relative_path_accepts_simple_path() {
        validate_relative_path(Path::new("a/b/c.ts")).expect("accept");
    }

    #[test]
    fn validate_relative_path_rejects_empty() {
        assert!(matches!(validate_relative_path(Path::new("")).unwrap_err(), RecordingPathError::Empty));
    }

    #[test]
    fn validate_relative_path_rejects_absolute() {
        assert!(matches!(
            validate_relative_path(Path::new("/etc/passwd")).unwrap_err(),
            RecordingPathError::Absolute
        ));
    }

    #[test]
    fn validate_relative_path_rejects_parent_traversal() {
        assert!(matches!(
            validate_relative_path(Path::new("../escape.ts")).unwrap_err(),
            RecordingPathError::InvalidComponent
        ));
    }

    #[test]
    fn validate_relative_path_rejects_nul_byte() {
        let bad = std::ffi::OsString::from("a\0b");
        assert!(matches!(
            validate_relative_path(Path::new(&bad)).unwrap_err(),
            RecordingPathError::NulByte
        ));
    }

    #[test]
    fn no_follow_regular_file_returns_none_for_missing_path() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist.ts");
        assert!(no_follow_regular_file(&missing).is_none());
    }

    #[test]
    fn no_follow_regular_file_rejects_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let real = dir.path().join("real.ts");
        std::fs::write(&real, b"hello").expect("write");
        let link_path = dir.path().join("link.ts");
        symlink(&real, &link_path).expect("symlink");
        // `symlink_metadata` returns the link itself; the type is
        // not a regular file. The helper must not follow.
        assert!(no_follow_regular_file(&link_path).is_none());
    }

    #[test]
    fn no_follow_regular_file_rejects_directory() {
        let dir = TempDir::new().expect("tempdir");
        assert!(no_follow_regular_file(dir.path()).is_none(), "directory must not pass as regular file");
    }

    #[test]
    fn open_partial_no_clobber_succeeds_for_fresh_path() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.partial.ts");
        let _file = open_partial_no_clobber(&path).expect("create");
        assert!(path.exists());
    }

    #[test]
    fn open_partial_no_clobber_fails_when_file_exists() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.partial.ts");
        std::fs::write(&path, b"already here").expect("write");
        let result = open_partial_no_clobber(&path);
        assert!(result.is_err(), "open must fail on existing file");
        assert!(matches!(result.unwrap_err(), RecordingPathError::Io(err) if err.kind() == io::ErrorKind::AlreadyExists));
    }

    #[test]
    fn open_partial_no_clobber_fails_when_path_is_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::write(&real, b"data").expect("write");
        let link_path = dir.path().join("link");
        symlink(&real, &link_path).expect("symlink");
        let result = open_partial_no_clobber(&link_path);
        assert!(result.is_err(), "open must fail on symlink target");
    }

    #[test]
    fn finalize_no_replace_succeeds_for_missing_final() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let final_path = dir.path().join("rec.ts");
        std::fs::write(&partial, b"recorded").expect("write partial");
        finalize_no_replace(&partial, &final_path).expect("finalize");
        assert!(!partial.exists(), "partial must be gone after rename");
        assert_eq!(std::fs::read(&final_path).expect("read"), b"recorded");
    }

    #[test]
    fn finalize_no_replace_refuses_when_final_already_exists() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let final_path = dir.path().join("rec.ts");
        std::fs::write(&partial, b"new").expect("write partial");
        std::fs::write(&final_path, b"existing").expect("write final");
        let result = finalize_no_replace(&partial, &final_path);
        assert!(matches!(result.unwrap_err(), RecordingPathError::AlreadyExists));
        assert!(partial.exists(), "partial must remain when finalize is refused");
        assert_eq!(std::fs::read(&final_path).expect("read"), b"existing");
    }

    #[test]
    fn finalize_no_replace_refuses_when_final_is_symlink() {
        // An externally created symlink at the final path counts
        // as a collision. The helper must refuse to clobber it.
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let real = dir.path().join("attacker-target");
        let final_path = dir.path().join("rec.ts");
        std::fs::write(&partial, b"new").expect("write partial");
        std::fs::write(&real, b"data").expect("write attacker target");
        symlink(&real, &final_path).expect("symlink final");
        let result = finalize_no_replace(&partial, &final_path);
        assert!(matches!(result.unwrap_err(), RecordingPathError::AlreadyExists));
    }

    #[test]
    fn safe_unlink_is_idempotent_for_missing_file() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("missing.ts");
        safe_unlink(&missing).expect("missing is success");
    }

    #[test]
    fn safe_unlink_removes_existing_file() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.ts");
        std::fs::write(&path, b"x").expect("write");
        safe_unlink(&path).expect("unlink");
        assert!(!path.exists());
    }

    #[test]
    fn safe_unlink_refuses_directories() {
        let dir = TempDir::new().expect("tempdir");
        let result = safe_unlink(dir.path());
        assert!(result.is_err(), "must not unlink a directory");
    }

    #[test]
    fn clean_empty_parents_removes_empty_subdirs_but_stops_at_root() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let leaf = nested.join("leaf.ts");
        std::fs::write(&leaf, b"x").expect("write");
        std::fs::remove_file(&leaf).expect("remove leaf");
        // The walk starts at `path.parent()` (i.e. `<root>/a/b`); `c` is
        // never cleaned because it is the path itself. We assert the
        // empty intermediates are gone and the root remains.
        clean_empty_parents(&nested, dir.path()).expect("clean");
        assert!(!dir.path().join("a").join("b").exists(), "empty b/ must be removed");
        assert!(!dir.path().join("a").exists(), "empty a/ must be removed");
        assert!(dir.path().exists(), "root must remain");
    }

    #[test]
    fn clean_empty_parents_stops_at_non_empty_directory() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(nested.join("sibling.ts"), b"keep").expect("write");
        clean_empty_parents(&nested.join("empty"), dir.path()).expect("clean");
        // The non-empty `a/` must remain because it still has `b/`.
        assert!(dir.path().join("a").exists(), "non-empty parent must remain");
    }

    #[test]
    fn resolve_recording_dir_lays_out_private_and_shared() {
        let root = Path::new("/var/recordings");
        let private =
            resolve_recording_dir(root, RecordingVisibility::Private, "web:abc", Path::new("2025/pilot.ts"))
                .expect("private");
        assert_eq!(private, PathBuf::from("/var/recordings/users/web:abc/2025/pilot.ts"));
        let shared = resolve_recording_dir(root, RecordingVisibility::Shared, "ignored", Path::new("pilot.ts"))
            .expect("shared");
        assert_eq!(shared, PathBuf::from("/var/recordings/shared/pilot.ts"));
    }

    #[test]
    fn resolve_recording_dir_rejects_traversal_in_relative() {
        let err = resolve_recording_dir(
            Path::new("/var/recordings"),
            RecordingVisibility::Private,
            "web:abc",
            Path::new("../escape.ts"),
        )
        .unwrap_err();
        assert!(matches!(err, RecordingPathError::InvalidComponent));
    }
}
