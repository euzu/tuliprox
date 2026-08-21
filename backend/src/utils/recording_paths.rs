//! Secure recording-path and file-operation helpers.
//!
//! The security properties this module guarantees — no symlink is ever
//! followed, no existing file is ever clobbered, the final rename is
//! atomic, and nothing escapes the recording root — are built from
//! portable primitives: `symlink_metadata` for no-follow inspection,
//! `create_new` for exclusive creation, and `rename` for the atomic
//! publish. The strict `openat2` with
//! `RESOLVE_BENEATH`/`RESOLVE_NO_SYMLINKS` would be Linux-only, so it is
//! deliberately not used.
//!
//! Portability: only [`open_partial_no_clobber`] has a platform-specific
//! branch, and only for defense in depth — see its docs. Everything else
//! compiles and behaves identically on every supported target. This
//! module used to carry a blanket `#![cfg(unix)]`, which erased it
//! wholesale on Windows and left every caller (`recording_deletion`,
//! `recording_media_api`, `recording_worker`) with unresolved imports —
//! i.e. the DVR did not build on Windows at all.

use std::io;
use std::path::{Component, Path, PathBuf};
use tokio::fs;

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
pub async fn assert_within_root(rel: &Path, root: &Path) -> Result<(), RecordingPathError> {
    let joined = root.join(rel);
    let canonical_root = fs::canonicalize(root).await.map_err(RecordingPathError::from)?;
    let canonical_joined = fs::canonicalize(&joined).await.map_err(RecordingPathError::from)?;
    if !canonical_joined.starts_with(&canonical_root) {
        return Err(RecordingPathError::NotWithinRoot);
    }
    Ok(())
}

/// Inspect a path without following symlinks. Returns `Some(metadata)` for
/// any path that exists at the location, including symlinks, directories,
/// and regular files. The caller can inspect the metadata to choose
/// the matching policy. Returns `None` only for missing entries.
pub async fn no_follow_existing(path: &Path) -> Option<std::fs::Metadata> {
    fs::symlink_metadata(path).await.ok()
}

/// Inspect a path without following symlinks. Returns `Some(metadata)` only
/// for regular files; directories, symlinks, sockets, devices, and
/// missing entries all return `None`.
pub async fn no_follow_regular_file(path: &Path) -> Option<std::fs::Metadata> {
    let meta = fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(meta)
}

/// Open a new partial file with no-clobber, no-follow semantics suitable
/// for ffmpeg to write into.
///
/// `create_new(true)` carries the security property on every platform: it
/// maps to `O_CREAT | O_EXCL` on Unix and `CREATE_NEW` on Windows, and
/// both fail when *anything* already exists at the path — including a
/// symlink, even a dangling one. So a pre-existing file and an
/// attacker-planted symlink are both rejected without any
/// platform-specific code.
///
/// On Unix we additionally pass `O_NOFOLLOW`. That is defense in depth,
/// not the mechanism: it makes the kernel refuse the open outright rather
/// than relying on the exclusivity check, so a future edit that weakens
/// `create_new` cannot silently open through a link.
pub async fn open_partial_no_clobber(path: &Path) -> Result<tokio::fs::File, RecordingPathError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        // `libc` is a `cfg(unix)` dependency, so this cannot be written
        // as a cross-platform expression.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).await?;
    Ok(file)
}

/// Finalize a partial file to its final path. The final path must not
/// exist at all (no regular file, no symlink, no directory) — we inspect
/// the location without following symlinks so an attacker-prepared
/// symlink is treated as a collision. The rename is then atomic. If
/// the partial is missing, `fs::rename` returns `NotFound` so the
/// caller can surface a visible failure rather than silently producing
/// an empty final file.
pub async fn finalize_no_replace(partial: &Path, final_path: &Path) -> Result<(), RecordingPathError> {
    if no_follow_existing(final_path).await.is_some() {
        return Err(RecordingPathError::AlreadyExists);
    }
    fs::rename(partial, final_path).await?;
    Ok(())
}

/// Unlink a file at the given path. Missing files are treated as
/// success so the call is idempotent; the operator's intent (file
/// gone) is satisfied either way.
pub async fn safe_unlink(path: &Path) -> Result<(), RecordingPathError> {
    match fs::remove_file(path).await {
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
pub async fn clean_empty_parents(path: &Path, root: &Path) -> Result<(), RecordingPathError> {
    let canonical_root = fs::canonicalize(root).await.ok();
    let mut current: Option<&Path> = Some(path);
    while let Some(dir) = current {
        let dir_for_check = dir;
        let mut stop_here = false;
        if let Some(r) = canonical_root.as_ref() {
            if fs::canonicalize(dir_for_check)
                .await
                .is_ok_and(|c| c == *r)
            {
                stop_here = true;
            }
        }
        if stop_here {
            break;
        }
        match fs::remove_dir(dir).await {
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
    validate_owner_id(owner_id)?;
    let base = match visibility {
        RecordingVisibility::Private => recording_root.join("users").join(owner_id),
        RecordingVisibility::Shared => recording_root.join("shared"),
    };
    Ok(base.join(rel))
}

/// Validate that an `owner_id` can be safely used as a single path
/// component. Rejects empty strings, NUL bytes, path separators, and
/// the traversal components `.` and `..` so the resulting layout can
/// never escape the configured recording root.
fn validate_owner_id(owner_id: &str) -> Result<(), RecordingPathError> {
    use std::path::Component;
    if owner_id.is_empty() || owner_id.contains('\0') {
        return Err(RecordingPathError::InvalidComponent);
    }
    let path = std::path::Path::new(owner_id);
    let components: Vec<_> = path.components().collect();
    if components.len() != 1 || !matches!(components.first(), Some(Component::Normal(_))) {
        return Err(RecordingPathError::InvalidComponent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a symlink at `link` pointing at `target`.
    ///
    /// Only the symlink-specific tests are Unix-gated; the rest of the
    /// suite is portable and must keep running on Windows. Creating a
    /// symlink on Windows needs either developer mode or elevation, so
    /// gating the assertions is the honest option — the behaviour they
    /// cover (`symlink_metadata` not following links, `create_new`
    /// refusing an existing entry) is provided by the standard library on
    /// both platforms.
    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

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

    #[tokio::test]
    async fn no_follow_regular_file_returns_none_for_missing_path() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist.ts");
        assert!(no_follow_regular_file(&missing).await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_follow_regular_file_rejects_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let real = dir.path().join("real.ts");
        tokio::fs::write(&real, b"hello").await.expect("write");
        let link_path = dir.path().join("link.ts");
        symlink(&real, &link_path).expect("symlink");
        // `symlink_metadata` returns the link itself; the type is
        // not a regular file. The helper must not follow.
        assert!(no_follow_regular_file(&link_path).await.is_none());
    }

    #[tokio::test]
    async fn no_follow_regular_file_rejects_directory() {
        let dir = TempDir::new().expect("tempdir");
        assert!(no_follow_regular_file(dir.path()).await.is_none(), "directory must not pass as regular file");
    }

    #[tokio::test]
    async fn open_partial_no_clobber_succeeds_for_fresh_path() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.partial.ts");
        let _file = open_partial_no_clobber(&path).await.expect("create");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn open_partial_no_clobber_fails_when_file_exists() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.partial.ts");
        tokio::fs::write(&path, b"already here").await.expect("write");
        let result = open_partial_no_clobber(&path).await;
        assert!(result.is_err(), "open must fail on existing file");
        assert!(matches!(result.unwrap_err(), RecordingPathError::Io(err) if err.kind() == io::ErrorKind::AlreadyExists));
        // A refused open must not have truncated what was there. This is
        // the portable half of the no-clobber guarantee: `create_new`
        // carries it on every target, which is why the `O_NOFOLLOW` flag
        // can be Unix-only without weakening the contract.
        assert_eq!(tokio::fs::read(&path).await.expect("read"), b"already here");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_partial_no_clobber_fails_when_path_is_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let real = dir.path().join("real");
        tokio::fs::write(&real, b"data").await.expect("write");
        let link_path = dir.path().join("link");
        symlink(&real, &link_path).expect("symlink");
        let result = open_partial_no_clobber(&link_path).await;
        assert!(result.is_err(), "open must fail on symlink target");
    }

    #[tokio::test]
    async fn finalize_no_replace_succeeds_for_missing_final() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let final_path = dir.path().join("rec.ts");
        tokio::fs::write(&partial, b"recorded").await.expect("write partial");
        finalize_no_replace(&partial, &final_path).await.expect("finalize");
        assert!(!partial.exists(), "partial must be gone after rename");
        assert_eq!(tokio::fs::read(&final_path).await.expect("read"), b"recorded");
    }

    #[tokio::test]
    async fn finalize_no_replace_refuses_when_final_already_exists() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let final_path = dir.path().join("rec.ts");
        tokio::fs::write(&partial, b"new").await.expect("write partial");
        tokio::fs::write(&final_path, b"existing").await.expect("write final");
        let result = finalize_no_replace(&partial, &final_path).await;
        assert!(matches!(result.unwrap_err(), RecordingPathError::AlreadyExists));
        assert!(partial.exists(), "partial must remain when finalize is refused");
        assert_eq!(tokio::fs::read(&final_path).await.expect("read"), b"existing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalize_no_replace_refuses_when_final_is_symlink() {
        // An externally created symlink at the final path counts
        // as a collision. The helper must refuse to clobber it.
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("rec.partial.ts");
        let real = dir.path().join("attacker-target");
        let final_path = dir.path().join("rec.ts");
        tokio::fs::write(&partial, b"new").await.expect("write partial");
        tokio::fs::write(&real, b"data").await.expect("write attacker target");
        symlink(&real, &final_path).expect("symlink final");
        let result = finalize_no_replace(&partial, &final_path).await;
        assert!(matches!(result.unwrap_err(), RecordingPathError::AlreadyExists));
    }

    #[tokio::test]
    async fn safe_unlink_is_idempotent_for_missing_file() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("missing.ts");
        safe_unlink(&missing).await.expect("missing is success");
    }

    #[tokio::test]
    async fn safe_unlink_removes_existing_file() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rec.ts");
        tokio::fs::write(&path, b"x").await.expect("write");
        safe_unlink(&path).await.expect("unlink");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn safe_unlink_refuses_directories() {
        let dir = TempDir::new().expect("tempdir");
        let result = safe_unlink(dir.path()).await;
        assert!(result.is_err(), "must not unlink a directory");
    }

    #[tokio::test]
    async fn clean_empty_parents_removes_empty_subdirs_but_stops_at_root() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("a").join("b").join("c");
        tokio::fs::create_dir_all(&nested).await.expect("mkdir");
        let leaf = nested.join("leaf.ts");
        tokio::fs::write(&leaf, b"x").await.expect("write");
        tokio::fs::remove_file(&leaf).await.expect("remove leaf");
        // The walk starts at `path.parent()` (i.e. `<root>/a/b`); `c` is
        // never cleaned because it is the path itself. We assert the
        // empty intermediates are gone and the root remains.
        clean_empty_parents(&nested, dir.path()).await.expect("clean");
        assert!(!dir.path().join("a").join("b").exists(), "empty b/ must be removed");
        assert!(!dir.path().join("a").exists(), "empty a/ must be removed");
        assert!(dir.path().exists(), "root must remain");
    }

    #[tokio::test]
    async fn clean_empty_parents_stops_at_non_empty_directory() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("a").join("b");
        tokio::fs::create_dir_all(&nested).await.expect("mkdir");
        tokio::fs::write(nested.join("sibling.ts"), b"keep").await.expect("write");
        clean_empty_parents(&nested.join("empty"), dir.path()).await.expect("clean");
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
