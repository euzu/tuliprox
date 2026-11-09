use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shared::model::{
    parse_input_override_file, parse_target_override_file, FieldError, InputOverrideFile, ItemOverridePatch,
    OverrideItemKind, OverrideParseError, TargetOverrideFile,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverrideNameError {
    #[error("override file name cannot be empty")]
    Empty,
    #[error("override file name exceeds maximum length of 240 bytes: length is {0}")]
    TooLong(usize),
}

/// Percent-encodes every byte outside `[A-Za-z0-9_-]` as `%XX` (uppercase hex).
///
/// # Errors
/// Returns [`OverrideNameError::Empty`] if `name` is empty.
/// Returns [`OverrideNameError::TooLong`] if the encoded string exceeds 240 bytes.
pub fn encode_override_file_name(name: &str) -> Result<String, OverrideNameError> {
    if name.is_empty() {
        return Err(OverrideNameError::Empty);
    }

    let mut encoded = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            encoded.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{b:02X}");
        }
    }

    if encoded.len() > 240 {
        return Err(OverrideNameError::TooLong(encoded.len()));
    }

    Ok(encoded)
}

/// Decodes an encoded override file name, returning `None` on invalid percent sequences or invalid UTF-8.
#[must_use]
pub fn decode_override_file_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }

    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let h1 = char::from(bytes[i + 1]).to_digit(16)?;
            let h2 = char::from(bytes[i + 2]).to_digit(16)?;
            let byte_val = u8::try_from((h1 << 4) | h2).ok()?;
            decoded.push(byte_val);
            i += 3;
        } else if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            decoded.push(b);
            i += 1;
        } else {
            return None;
        }
    }

    String::from_utf8(decoded).ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OverrideRevision(pub String);

impl OverrideRevision {
    #[must_use]
    pub fn empty() -> Self { Self(blake3::hash(b"").to_hex().to_string()) }

    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self { Self(blake3::hash(bytes).to_hex().to_string()) }

    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for OverrideRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideScope {
    Input(String),
    Target(String),
}

impl OverrideScope {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Input(n) | Self::Target(n) => n,
        }
    }

    #[must_use]
    pub fn sub_dir(&self) -> &'static str {
        match self {
            Self::Input(_) => "input",
            Self::Target(_) => "target",
        }
    }

    /// Resolves the filesystem path for this scope within `base_dir`.
    ///
    /// # Errors
    /// Returns [`OverrideNameError`] if the scope name cannot be encoded.
    pub fn file_path(&self, base_dir: &Path) -> Result<PathBuf, OverrideNameError> {
        let enc = encode_override_file_name(self.name())?;
        Ok(base_dir.join(self.sub_dir()).join(format!("{enc}.yml")))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverridePatchItem {
    pub kind: OverrideItemKind,
    pub input: Option<String>,
    pub id: String,
    pub patch: ItemOverridePatch,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedOverrideFile {
    Input(InputOverrideFile),
    Target(TargetOverrideFile),
}

impl ParsedOverrideFile {
    #[must_use]
    pub fn as_input(&self) -> Option<&InputOverrideFile> {
        match self {
            Self::Input(f) => Some(f),
            Self::Target(_) => None,
        }
    }

    #[must_use]
    pub fn as_target(&self) -> Option<&TargetOverrideFile> {
        match self {
            Self::Target(f) => Some(f),
            Self::Input(_) => None,
        }
    }

    #[must_use]
    pub fn into_input(self) -> Option<InputOverrideFile> {
        match self {
            Self::Input(f) => Some(f),
            Self::Target(_) => None,
        }
    }

    #[must_use]
    pub fn into_target(self) -> Option<TargetOverrideFile> {
        match self {
            Self::Target(f) => Some(f),
            Self::Input(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OverrideParseResult {
    Valid { file: ParsedOverrideFile, field_errors: Vec<FieldError> },
    Malformed { error: String },
    UnsupportedSchema { version: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedOverrides {
    pub revision: OverrideRevision,
    pub parsed: OverrideParseResult,
}

#[derive(Debug, thiserror::Error)]
pub enum OverrideWriteError {
    #[error("revision conflict: current revision is {current:?}")]
    Conflict { current: OverrideRevision },
    #[error("cannot patch malformed override file: {error}")]
    Malformed { error: String },
    #[error("cannot patch override file with unsupported schema version: {version}")]
    UnsupportedSchema { version: u32 },
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
    #[error("invalid override file name: {0}")]
    InvalidName(#[from] OverrideNameError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
}

enum WorkingFile {
    Input(InputOverrideFile),
    Target(TargetOverrideFile),
}

#[cfg(test)]
pub type PreRenameHook = Arc<
    dyn Fn(&Path) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OverrideWriteError>> + Send>>
        + Send
        + Sync,
>;

pub struct OverrideRepository {
    dir: PathBuf,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    #[cfg(test)]
    pub(crate) pre_rename_hook: Mutex<Option<PreRenameHook>>,
    #[cfg(test)]
    pub(crate) fail_dir_sync: std::sync::atomic::AtomicBool,
}

impl OverrideRepository {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            locks: Mutex::new(HashMap::new()),
            #[cfg(test)]
            pre_rename_hook: Mutex::new(None),
            #[cfg(test)]
            fail_dir_sync: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path { &self.dir }

    /// Resolves the file path for an override scope within this repository.
    ///
    /// # Errors
    /// Returns [`OverrideNameError`] if encoding the scope name fails.
    pub fn file_path(&self, scope: &OverrideScope) -> Result<PathBuf, OverrideNameError> { scope.file_path(&self.dir) }

    fn get_file_lock(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock();
        locks.entry(path.to_path_buf()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    #[cfg(test)]
    pub fn set_pre_rename_hook<F, Fut>(&self, hook: F)
    where
        F: Fn(&Path) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), OverrideWriteError>> + Send + 'static,
    {
        *self.pre_rename_hook.lock() = Some(Arc::new(move |path| Box::pin(hook(path))));
    }

    #[cfg(test)]
    pub fn clear_pre_rename_hook(&self) { *self.pre_rename_hook.lock() = None; }

    /// Loads the overrides for `scope`, returning the content revision and parsed result.
    ///
    /// # Errors
    /// Returns `std::io::Error` on filesystem read failures other than `NotFound`.
    pub async fn load(&self, scope: &OverrideScope) -> std::io::Result<LoadedOverrides> {
        let path = match self.file_path(scope) {
            Ok(p) => p,
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())),
        };

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let empty_file = match scope {
                    OverrideScope::Input(_) => ParsedOverrideFile::Input(InputOverrideFile::default()),
                    OverrideScope::Target(_) => ParsedOverrideFile::Target(TargetOverrideFile::default()),
                };
                return Ok(LoadedOverrides {
                    revision: OverrideRevision::empty(),
                    parsed: OverrideParseResult::Valid { file: empty_file, field_errors: Vec::new() },
                });
            }
            Err(err) => return Err(err),
        };

        let revision = OverrideRevision::from_bytes(&bytes);
        let Ok(yaml_str) = std::str::from_utf8(&bytes) else {
            return Ok(LoadedOverrides {
                revision,
                parsed: OverrideParseResult::Malformed { error: "file is not valid UTF-8".to_string() },
            });
        };

        let parsed = match scope {
            OverrideScope::Input(_) => match parse_input_override_file(yaml_str) {
                Ok((file, field_errors)) => {
                    OverrideParseResult::Valid { file: ParsedOverrideFile::Input(file), field_errors }
                }
                Err(OverrideParseError::Malformed(error)) => OverrideParseResult::Malformed { error },
                Err(OverrideParseError::UnsupportedSchema(version)) => {
                    OverrideParseResult::UnsupportedSchema { version }
                }
            },
            OverrideScope::Target(_) => match parse_target_override_file(yaml_str) {
                Ok((file, field_errors)) => {
                    OverrideParseResult::Valid { file: ParsedOverrideFile::Target(file), field_errors }
                }
                Err(OverrideParseError::Malformed(error)) => OverrideParseResult::Malformed { error },
                Err(OverrideParseError::UnsupportedSchema(version)) => {
                    OverrideParseResult::UnsupportedSchema { version }
                }
            },
        };

        Ok(LoadedOverrides { revision, parsed })
    }

    /// Applies patches to the override file for `scope`.
    ///
    /// # Errors
    /// Returns [`OverrideWriteError`] on conflict, malformed file, unsupported schema, invalid patch, or I/O failure.
    pub async fn apply_patches(
        &self,
        scope: &OverrideScope,
        base: &OverrideRevision,
        patches: Vec<OverridePatchItem>,
    ) -> Result<OverrideRevision, OverrideWriteError> {
        let path = self.file_path(scope)?;
        let file_lock = self.get_file_lock(&path);
        let _guard = file_lock.lock().await;

        validate_scope_patches(scope, &patches)?;

        let (current_bytes, current_revision) = read_current_file(&path).await?;
        if base != &current_revision {
            return Err(OverrideWriteError::Conflict { current: current_revision });
        }

        let mut working_file = parse_existing_or_default(scope, current_bytes.as_deref())?;
        let is_empty = apply_to_working_file(&mut working_file, patches)?;

        if is_empty {
            self.delete_empty_file(&path, &current_revision).await?;
            return Ok(OverrideRevision::empty());
        }

        let serialized = serialize_working_file(&working_file)?;
        let new_revision = OverrideRevision::from_bytes(serialized.as_bytes());

        self.write_atomic_and_sync(&path, &serialized, &current_revision).await?;
        Ok(new_revision)
    }

    /// Removes the whole override file of `scope`, provided `base` is still its revision.
    ///
    /// The revision is compared under the same per-file lock [`Self::apply_patches`] takes, so a
    /// concurrent writer is never silently discarded. A file that is already gone answers with the
    /// revision of an absent file, which makes a repeated discard idempotent.
    ///
    /// # Errors
    /// Returns [`OverrideWriteError::Conflict`] when the file changed since `base`, and
    /// [`OverrideWriteError`] on an invalid name or I/O failure.
    pub async fn delete_file(
        &self,
        scope: &OverrideScope,
        base: &OverrideRevision,
    ) -> Result<OverrideRevision, OverrideWriteError> {
        let path = self.file_path(scope)?;
        let file_lock = self.get_file_lock(&path);
        let _guard = file_lock.lock().await;

        let (_current_bytes, current_revision) = read_current_file(&path).await?;
        if base != &current_revision {
            return Err(OverrideWriteError::Conflict { current: current_revision });
        }

        self.delete_empty_file(&path, &current_revision).await?;
        Ok(OverrideRevision::empty())
    }

    async fn delete_empty_file(
        &self,
        path: &Path,
        current_revision: &OverrideRevision,
    ) -> Result<(), OverrideWriteError> {
        #[cfg(test)]
        {
            let hook_opt = self.pre_rename_hook.lock().clone();
            if let Some(hook) = hook_opt {
                hook(path).await?;
            }
        }

        let disk_rev = match tokio::fs::read(path).await {
            Ok(b) => OverrideRevision::from_bytes(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => OverrideRevision::empty(),
            Err(e) => return Err(OverrideWriteError::Io(e)),
        };
        if disk_rev != *current_revision {
            return Err(OverrideWriteError::Conflict { current: disk_rev });
        }

        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(OverrideWriteError::Io(e)),
        }

        if let Some(parent) = path.parent() {
            if parent.exists() {
                if let Err(err) = sync_dir(parent).await {
                    log::warn!("Failed to sync parent directory {}: {err}", parent.display());
                }
            }
        }
        Ok(())
    }

    async fn write_atomic_and_sync(
        &self,
        path: &Path,
        serialized: &str,
        current_revision: &OverrideRevision,
    ) -> Result<(), OverrideWriteError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await.map_err(OverrideWriteError::Io)?;

        let temp_path = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));

        let write_result: Result<(), OverrideWriteError> = async {
            use tokio::io::AsyncWriteExt;
            let mut temp_file = tokio::fs::File::create(&temp_path).await.map_err(OverrideWriteError::Io)?;
            temp_file.write_all(serialized.as_bytes()).await.map_err(OverrideWriteError::Io)?;
            temp_file.sync_all().await.map_err(OverrideWriteError::Io)?;
            drop(temp_file);

            #[cfg(test)]
            {
                let hook_opt = self.pre_rename_hook.lock().clone();
                if let Some(hook) = hook_opt {
                    hook(path).await?;
                }
            }

            let disk_rev = match tokio::fs::read(path).await {
                Ok(b) => OverrideRevision::from_bytes(&b),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => OverrideRevision::empty(),
                Err(err) => return Err(OverrideWriteError::Io(err)),
            };
            if disk_rev != *current_revision {
                return Err(OverrideWriteError::Conflict { current: disk_rev });
            }

            tokio::fs::rename(&temp_path, path).await.map_err(OverrideWriteError::Io)?;

            #[cfg(test)]
            let dir_sync_failed =
                self.fail_dir_sync.load(std::sync::atomic::Ordering::Relaxed) || sync_dir(parent).await.is_err();
            #[cfg(not(test))]
            let dir_sync_failed = sync_dir(parent).await.is_err();

            if dir_sync_failed {
                log::warn!("Failed to sync parent directory {}", parent.display());
            }

            Ok(())
        }
        .await;

        if let Err(err) = write_result {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(err);
        }

        Ok(())
    }

    pub async fn list_files(&self) -> Vec<(OverrideScope, PathBuf)> {
        let mut seen = HashSet::new();
        let mut results = Vec::new();

        for &(sub, is_input) in &[("input", true), ("target", false)] {
            let sub_dir = self.dir.join(sub);
            let Ok(mut read_dir) = tokio::fs::read_dir(&sub_dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(decoded_name) = decode_override_file_name(stem) else {
                    continue;
                };
                if encode_override_file_name(&decoded_name).as_deref() != Ok(stem) {
                    continue;
                }
                let scope =
                    if is_input { OverrideScope::Input(decoded_name) } else { OverrideScope::Target(decoded_name) };
                if seen.insert(scope.clone()) {
                    results.push((scope, path));
                }
            }
        }

        results.sort_by(|(s1, p1), (s2, p2)| (s1.name(), p1).cmp(&(s2.name(), p2)));
        results
    }
}

fn validate_scope_patches(scope: &OverrideScope, patches: &[OverridePatchItem]) -> Result<(), OverrideWriteError> {
    for patch in patches {
        if patch.id.trim().is_empty() {
            return Err(OverrideWriteError::InvalidPatch("item id cannot be empty or blank".to_string()));
        }
    }

    match scope {
        OverrideScope::Input(scope_input_name) => {
            if scope_input_name.trim() != scope_input_name {
                return Err(OverrideWriteError::InvalidPatch(format!(
                    "input name cannot have surrounding whitespace: '{scope_input_name}'"
                )));
            }
            for patch in patches {
                if let Some(input_name) = &patch.input {
                    if input_name.trim() != input_name {
                        return Err(OverrideWriteError::InvalidPatch(format!(
                            "input name cannot have surrounding whitespace: '{input_name}'"
                        )));
                    }
                    if input_name != scope_input_name {
                        return Err(OverrideWriteError::InvalidPatch(format!(
                            "input override patch specifies input '{input_name}', but scope is '{scope_input_name}'"
                        )));
                    }
                }
            }
        }
        OverrideScope::Target(scope_target_name) => {
            if scope_target_name.trim() != scope_target_name {
                return Err(OverrideWriteError::InvalidPatch(format!(
                    "target name cannot have surrounding whitespace: '{scope_target_name}'"
                )));
            }
            for patch in patches {
                let Some(input_name) = &patch.input else {
                    return Err(OverrideWriteError::InvalidPatch(
                        "target override patch must carry an input name".to_string(),
                    ));
                };
                if input_name.trim().is_empty() {
                    return Err(OverrideWriteError::InvalidPatch(
                        "target override patch must carry a non-empty input name".to_string(),
                    ));
                }
                if input_name.trim() != input_name {
                    return Err(OverrideWriteError::InvalidPatch(format!(
                        "input name cannot have surrounding whitespace: '{input_name}'"
                    )));
                }
            }
        }
    }
    Ok(())
}

async fn read_current_file(path: &Path) -> Result<(Option<Vec<u8>>, OverrideRevision), OverrideWriteError> {
    match tokio::fs::read(path).await {
        Ok(b) => {
            let rev = OverrideRevision::from_bytes(&b);
            Ok((Some(b), rev))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((None, OverrideRevision::empty())),
        Err(err) => Err(OverrideWriteError::Io(err)),
    }
}

fn parse_existing_or_default(
    scope: &OverrideScope,
    current_bytes: Option<&[u8]>,
) -> Result<WorkingFile, OverrideWriteError> {
    match (scope, current_bytes) {
        (OverrideScope::Input(_), None) => Ok(WorkingFile::Input(InputOverrideFile::default())),
        (OverrideScope::Target(_), None) => Ok(WorkingFile::Target(TargetOverrideFile::default())),
        (OverrideScope::Input(_), Some(bytes)) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| OverrideWriteError::Malformed { error: "file is not valid UTF-8".to_string() })?;
            match parse_input_override_file(text) {
                Ok((file, _)) => Ok(WorkingFile::Input(file)),
                Err(OverrideParseError::Malformed(e)) => Err(OverrideWriteError::Malformed { error: e }),
                Err(OverrideParseError::UnsupportedSchema(v)) => {
                    Err(OverrideWriteError::UnsupportedSchema { version: v })
                }
            }
        }
        (OverrideScope::Target(_), Some(bytes)) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| OverrideWriteError::Malformed { error: "file is not valid UTF-8".to_string() })?;
            match parse_target_override_file(text) {
                Ok((file, _)) => Ok(WorkingFile::Target(file)),
                Err(OverrideParseError::Malformed(e)) => Err(OverrideWriteError::Malformed { error: e }),
                Err(OverrideParseError::UnsupportedSchema(v)) => {
                    Err(OverrideWriteError::UnsupportedSchema { version: v })
                }
            }
        }
    }
}

fn apply_to_working_file(
    working_file: &mut WorkingFile,
    patches: Vec<OverridePatchItem>,
) -> Result<bool, OverrideWriteError> {
    let is_empty = match working_file {
        WorkingFile::Input(file) => {
            for patch in patches {
                let section = match patch.kind {
                    OverrideItemKind::Live => &mut file.sections.live,
                    OverrideItemKind::Vod => &mut file.sections.vod,
                    OverrideItemKind::Series => &mut file.sections.series,
                };
                let item = section.entry(patch.id).or_default();
                item.apply_patch(&patch.patch);
                if !patch.hint.is_empty() {
                    item.hint = Some(patch.hint);
                }
            }
            file.sections.prune_empty();
            file.sections.live.is_empty() && file.sections.vod.is_empty() && file.sections.series.is_empty()
        }
        WorkingFile::Target(file) => {
            for patch in patches {
                let Some(input_name) = patch.input else {
                    return Err(OverrideWriteError::InvalidPatch(
                        "target override patch must carry an input name".to_string(),
                    ));
                };
                let sections = file.inputs.entry(input_name).or_default();
                let section = match patch.kind {
                    OverrideItemKind::Live => &mut sections.live,
                    OverrideItemKind::Vod => &mut sections.vod,
                    OverrideItemKind::Series => &mut sections.series,
                };
                let item = section.entry(patch.id).or_default();
                item.apply_patch(&patch.patch);
                if !patch.hint.is_empty() {
                    item.hint = Some(patch.hint);
                }
            }
            for sections in file.inputs.values_mut() {
                sections.prune_empty();
            }
            file.inputs.retain(|_, s| !s.live.is_empty() || !s.vod.is_empty() || !s.series.is_empty());
            file.inputs.is_empty()
        }
    };
    Ok(is_empty)
}

fn serialize_working_file(working_file: &WorkingFile) -> Result<String, OverrideWriteError> {
    match working_file {
        WorkingFile::Input(f) => {
            serde_saphyr::to_string(f).map_err(|e| OverrideWriteError::Serialization(e.to_string()))
        }
        WorkingFile::Target(f) => {
            serde_saphyr::to_string(f).map_err(|e| OverrideWriteError::Serialization(e.to_string()))
        }
    }
}

async fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let f = tokio::fs::File::open(dir).await?;
        f.sync_all().await?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
