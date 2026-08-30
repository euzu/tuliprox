//! Shared error-mapping macros for the repository crate.
//!
//! Every backend persists B+Tree-backed playlists through the same shape of
//! call site: read a tree from disk, mutate it, store it back, then bubble
//! `io::Error` / `JoinError` up as `TuliproxError::Repository<Variant>`. The
//! human-readable message wraps the file path and the underlying error.
//!
//! Historically each submodule (`m3u_repository`, `xtream_repository`) defined
//! its own private `cant_write_result!` macro with the variant baked in, which
//! meant the same boilerplate lived in two places and any new backend had to
//! copy-paste the format string verbatim. Centralising the macros here keeps
//! the format identical across backends and removes a class of
//! "fix-it-in-one-place-but-forget-the-other" drift.
//!
//! All call sites must supply the `TuliproxError` variant explicitly (e.g.
//! `RepositoryM3u`) so the macro cannot accidentally widen an error type.
//! `TuliproxError` is expected to be in scope at every call site (this crate
//! always uses it via `shared::error::TuliproxError`).

#[inline]
pub(crate) fn format_repo_playlist_err(
    action: &str,
    label: &str,
    path: &dyn std::fmt::Display,
    err: &dyn std::fmt::Display,
) -> String {
    format!("failed to {action} {label} playlist: {path} - {err}")
}

#[inline]
pub(crate) fn format_repo_db_err(
    action: &str,
    label: &str,
    path: &dyn std::fmt::Display,
    err: &dyn std::fmt::Display,
) -> String {
    format!("failed to {action} {label} db {path}: {err}")
}

/// Wrap an `io::Error` (or any `Display` value) into the canonical
/// "failed to write {label} playlist: {path} - {err}" message and produce the
/// matching `TuliproxError::Repository<Variant>`.
///
/// Use wherever a `BPlusTree::store` / `store_with_index` failure needs to be
/// promoted to a domain error. The `$label` is the short, lower-case
/// identifier for the backend (e.g. `"m3u"`, `"xtream"`).
///
/// Example:
/// ```ignore
/// tree.store(&path).map_err(|err| {
///     cant_write_result!(RepositoryM3u, "m3u", &path, err)
/// })?;
/// ```
macro_rules! cant_write_result {
    ($variant:ident, $label:literal, $path:expr, $err:expr $(,)?) => {{
        TuliproxError::$variant($crate::error_macros::format_repo_playlist_err(
            "write",
            $label,
            &($path).display(),
            &$err,
        ))
    }};
}

/// Await an async expression and promote a `JoinError` into the matching
/// `TuliproxError::Repository<Variant>`. The `$fmt` / `$args` mirror
/// `format!`; the join error is appended as the trailing format argument so
/// callers do not need to remember to interpolate it.
///
/// Example:
/// ```ignore
/// tokio::spawn(work).await.map_err(|err| {
///     await_playlist_write!(RepositoryM3u, expr, "failed to read m3u playlist: {}", path.display())
/// })??;
/// ```
macro_rules! await_playlist_write {
    ($variant:ident, $expr:expr, $fmt:literal $(, $args:expr)* $(,)?) => {{
        $expr.await.map_err(|err| {
            TuliproxError::$variant(format!($fmt $(, $args)*, err))
        })?
    }};
}

/// Wrap an `io::Error` (or any `Display` value) into the canonical
/// "failed to read {label} playlist: {path} - {err}" message and produce the
/// matching `TuliproxError::Repository<Variant>`.
macro_rules! cant_read_result {
    ($variant:ident, $label:literal, $path:expr, $err:expr $(,)?) => {{
        TuliproxError::$variant($crate::error_macros::format_repo_playlist_err(
            "read",
            $label,
            &($path).display(),
            &$err,
        ))
    }};
}

/// Wrap an `io::Error` (or any `Display` value) into the canonical
/// "failed to open {label} db {path}: {err}" message and produce the
/// matching `TuliproxError::Repository<Variant>`.
macro_rules! cant_open_result {
    ($variant:ident, $label:literal, $path:expr, $err:expr $(,)?) => {{
        TuliproxError::$variant($crate::error_macros::format_repo_db_err("open", $label, &($path).display(), &$err))
    }};
}

/// Wrap an `io::Error` (or any `Display` value) into the canonical
/// "failed to query {label} db {path}: {err}" message and produce the
/// matching `TuliproxError::Repository<Variant>`.
macro_rules! cant_query_result {
    ($variant:ident, $label:literal, $path:expr, $err:expr $(,)?) => {{
        TuliproxError::$variant($crate::error_macros::format_repo_db_err("query", $label, &($path).display(), &$err))
    }};
}

pub(crate) use await_playlist_write;
pub(crate) use cant_open_result;
pub(crate) use cant_query_result;
pub(crate) use cant_read_result;
pub(crate) use cant_write_result;
