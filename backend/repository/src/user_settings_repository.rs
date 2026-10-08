use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use shared::model::{
    settings_id_valid, TableLayoutPreferencesDto, UserId, UserSettingsDto, SETTINGS_MAX_FILE_BYTES, SETTINGS_MAX_TABLES,
};
use std::{
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;
use tuliprox_core::utils::{atomic_json_store::write_file_atomic, FileLockManager};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsOwner {
    pub subject_id: String,
    pub username: String,
}

impl SettingsOwner {
    pub fn local() -> Self { Self { subject_id: "local:no_auth".into(), username: "no_auth".into() } }

    pub fn authenticated(subject: &UserId, username: &str) -> Result<Self, SettingsError> {
        if username.is_empty()
            || !(subject.is_builtin_admin() || *subject == UserId::web(username) || *subject == UserId::api(username))
        {
            return Err(SettingsError::OwnerMismatch);
        }
        Ok(Self { subject_id: subject.to_string(), username: username.to_owned() })
    }

    fn namespace(&self) -> Result<&str, SettingsError> {
        if self.subject_id == "local:no_auth" && self.username == "no_auth" {
            return Ok("local");
        }
        if self.subject_id == UserId::BUILTIN_ADMIN_NAMESPACE {
            return Ok("builtin");
        }
        let (namespace, subject) = if self.subject_id.starts_with(UserId::WEB_NAMESPACE) {
            ("web", UserId::web(&self.username))
        } else if self.subject_id.starts_with(UserId::API_NAMESPACE) {
            ("api", UserId::api(&self.username))
        } else {
            return Err(SettingsError::OwnerMismatch);
        };
        if subject.0 == self.subject_id {
            return Ok(namespace);
        }
        Err(SettingsError::OwnerMismatch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("settings_invalid")]
    Invalid,
    #[error("settings_version_unsupported")]
    VersionUnsupported,
    #[error("settings_owner_mismatch")]
    OwnerMismatch,
    #[error("settings_file_unsafe")]
    UnsafeFile,
    #[error("settings_io_error")]
    Io,
    #[error("settings_limits_exceeded")]
    Limits,
    #[error("settings_limits_exceeded")]
    StoredLimits,
    #[error("settings_payload_invalid")]
    PayloadInvalid,
    #[error("settings_id_invalid")]
    IdInvalid,
    #[error("settings_precondition_failed")]
    PreconditionFailed,
    #[error("settings_precondition_required")]
    PreconditionRequired,
    #[error("settings_precondition_invalid")]
    PreconditionInvalid,
}

impl SettingsError {
    pub const fn status(&self) -> u16 {
        match self {
            Self::Io => 500,
            Self::Invalid | Self::VersionUnsupported | Self::OwnerMismatch | Self::UnsafeFile | Self::StoredLimits => {
                409
            }
            Self::PreconditionFailed => 412,
            Self::PreconditionRequired => 428,
            _ => 400,
        }
    }
}

fn io_error(err: &io::Error) -> SettingsError {
    log::warn!("User settings I/O failed: {err}");
    SettingsError::Io
}

fn task_error(err: &tokio::task::JoinError) -> SettingsError {
    log::warn!("User settings task failed: {err}");
    SettingsError::Io
}

/// Encodes configured names independently of opaque subject identifiers.
pub fn settings_filename(username: &str) -> Result<String, SettingsError> {
    if username.is_empty() {
        return Err(SettingsError::OwnerMismatch);
    }
    let mut stem = String::with_capacity(username.len().min(180));
    for byte in username.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-') {
            stem.push(char::from(byte));
        } else {
            let _ = write!(stem, "%{byte:02X}");
        }
        if stem.len() > 180 {
            break;
        }
    }
    let reserved = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if stem.len() > 180 || reserved {
        let mut prefix_len = stem.len().min(80);
        // Keep encoded bytes intact so existing filenames remain stable.
        if let Some(escape) = stem[..prefix_len].rfind('%') {
            if prefix_len - escape < 3 {
                prefix_len = escape;
            }
        }
        stem.truncate(prefix_len);
        let _ = write!(stem, "~{}", blake3::hash(username.as_bytes()).to_hex());
    }
    Ok(format!("{stem}.yml"))
}

pub fn settings_path(config_path: &Path, owner: &SettingsOwner) -> Result<PathBuf, SettingsError> {
    Ok(config_path.join("user_settings").join(owner.namespace()?).join(settings_filename(&owner.username)?))
}

async fn safe_path(path: &Path) -> Result<(), SettingsError> {
    // The config directory is trusted; settings directories and files cannot be links.
    for candidate in path.ancestors().take(3) {
        match tokio::fs::symlink_metadata(candidate).await {
            Ok(meta)
                if meta.file_type().is_symlink()
                    || (candidate == path && !meta.is_file())
                    || (candidate != path && !meta.is_dir()) =>
            {
                return Err(SettingsError::UnsafeFile)
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(&err)),
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct UserSettingsEntity {
    document: Value,
}

impl UserSettingsEntity {
    fn empty(owner: &SettingsOwner) -> Self {
        Self { document: json!({ "version": 1, "owner": owner, "preferences": {} }) }
    }

    fn tables(&self) -> Option<&Map<String, Value>> {
        self.document.pointer("/preferences/web_ui/tables").and_then(Value::as_object)
    }

    pub fn layout(&self, table: &str) -> Result<TableLayoutPreferencesDto, SettingsError> {
        let Some(value) = self.tables().and_then(|tables| tables.get(table)) else {
            return Ok(TableLayoutPreferencesDto::default());
        };
        // Decode known fields by reference; the document retains unknown storage fields.
        let layout = TableLayoutPreferencesDto {
            column_order: value
                .get("column_order")
                .map(Deserialize::deserialize)
                .transpose()
                .map_err(|_| SettingsError::Invalid)?
                .unwrap_or_default(),
            column_visibility: value
                .get("column_visibility")
                .map(Deserialize::deserialize)
                .transpose()
                .map_err(|_| SettingsError::Invalid)?
                .unwrap_or_default(),
        };
        layout.validate(false).map_err(|code| {
            if code == "settings_limits_exceeded" {
                SettingsError::Limits
            } else {
                SettingsError::Invalid
            }
        })?;
        Ok(layout)
    }

    pub fn etag(&self, owner: &SettingsOwner, table: &str) -> Result<String, SettingsError> {
        self.etag_with_layout(owner, table, &self.layout(table)?)
    }

    pub fn etag_with_layout(
        &self,
        owner: &SettingsOwner,
        table: &str,
        layout: &TableLayoutPreferencesDto,
    ) -> Result<String, SettingsError> {
        let value = self.tables().and_then(|tables| tables.get(table));
        let presence = ["column_order", "column_visibility"].map(|key| value.is_some_and(|v| v.get(key).is_some()));
        let mut hasher = blake3::Hasher::new();
        serde_json::to_writer(&mut hasher, &(&owner.subject_id, &owner.username, table, presence, layout))
            .map_err(|_| SettingsError::Invalid)?;
        Ok(format!("\"{}\"", hasher.finalize().to_hex()))
    }

    pub fn dto(&self, owner: &SettingsOwner) -> Result<UserSettingsDto, SettingsError> {
        let mut result = UserSettingsDto { shared: owner == &SettingsOwner::local(), ..UserSettingsDto::default() };
        for table in self.tables().into_iter().flat_map(Map::keys) {
            let layout = self.layout(table)?;
            result.section_etags.insert(table.into(), self.etag_with_layout(owner, table, &layout)?);
            result.preferences.web_ui.tables.insert(table.clone(), layout);
        }
        Ok(result)
    }

    fn validate(&self, owner: &SettingsOwner) -> Result<(), SettingsError> {
        if !self.document.is_object() {
            return Err(SettingsError::Invalid);
        }
        match self.document.get("version").and_then(Value::as_u64) {
            Some(1) => {}
            Some(version) if version > 1 => return Err(SettingsError::VersionUnsupported),
            _ => return Err(SettingsError::Invalid),
        }
        let stored: SettingsOwner =
            serde_json::from_value(self.document.get("owner").cloned().ok_or(SettingsError::Invalid)?)
                .map_err(|_| SettingsError::Invalid)?;
        if &stored != owner {
            return Err(SettingsError::OwnerMismatch);
        }
        for pointer in ["/preferences", "/preferences/web_ui", "/preferences/web_ui/tables"] {
            if self.document.pointer(pointer).is_some_and(|v| !v.is_object()) {
                return Err(SettingsError::Invalid);
            }
        }
        if let Some(tables) = self.tables() {
            if tables.len() > SETTINGS_MAX_TABLES {
                return Err(SettingsError::Limits);
            }
            for (table, value) in tables {
                if !settings_id_valid(table) || !value.is_object() {
                    return Err(SettingsError::Invalid);
                }
                self.layout(table)?;
            }
        }
        Ok(())
    }

    fn update(&mut self, table: &str, layout: Option<&TableLayoutPreferencesDto>) -> Result<(), SettingsError> {
        let mut node = &mut self.document;
        for key in ["preferences", "web_ui", "tables"] {
            node = node.as_object_mut().ok_or(SettingsError::Invalid)?.entry(key).or_insert_with(|| json!({}));
        }
        let tables = node.as_object_mut().ok_or(SettingsError::Invalid)?;
        if let Some(layout) = layout {
            let fields =
                tables.entry(table).or_insert_with(|| json!({})).as_object_mut().ok_or(SettingsError::Invalid)?;
            fields.insert("column_order".into(), json!(layout.column_order));
            fields.insert("column_visibility".into(), json!(layout.column_visibility));
        } else if let Some(fields) = tables.get_mut(table).and_then(Value::as_object_mut) {
            fields.remove("column_order");
            fields.remove("column_visibility");
            if fields.is_empty() {
                tables.remove(table);
            }
        }
        Ok(())
    }
}

fn parse_document(content: &[u8], owner: &SettingsOwner) -> Result<UserSettingsEntity, SettingsError> {
    let text = std::str::from_utf8(content).map_err(|_| SettingsError::Invalid)?;
    let mut depth = 0usize;
    for event in saphyr_parser::Parser::new_from_str(text) {
        let (event, _) = event.map_err(|_| SettingsError::Invalid)?;
        match event {
            saphyr_parser::Event::Scalar(_, _, _, Some(_))
            | saphyr_parser::Event::MappingStart(_, Some(_))
            | saphyr_parser::Event::SequenceStart(_, Some(_))
            | saphyr_parser::Event::Alias(_) => return Err(SettingsError::Invalid),
            saphyr_parser::Event::MappingStart(_, _) | saphyr_parser::Event::SequenceStart(_, _) => {
                depth += 1;
                if depth > 16 {
                    return Err(SettingsError::Limits);
                }
            }
            saphyr_parser::Event::MappingEnd | saphyr_parser::Event::SequenceEnd => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! { max_depth: 16, max_aliases: 0, max_events: 100_000, max_documents: 1 },
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
    };
    let document = serde_saphyr::from_slice_with_options(content, options).map_err(|_| SettingsError::Invalid)?;
    let result = UserSettingsEntity { document };
    result.validate(owner)?;
    Ok(result)
}

pub async fn load_settings_unlocked(path: &Path, owner: &SettingsOwner) -> Result<UserSettingsEntity, SettingsError> {
    safe_path(path).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path).await {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(UserSettingsEntity::empty(owner)),
        Err(err) => return Err(io_error(&err)),
    };
    if !file.metadata().await.map_err(|err| io_error(&err))?.is_file() {
        return Err(SettingsError::UnsafeFile);
    }
    let mut content = Vec::new();
    file.take((SETTINGS_MAX_FILE_BYTES + 1) as u64).read_to_end(&mut content).await.map_err(|err| io_error(&err))?;
    if content.len() > SETTINGS_MAX_FILE_BYTES {
        return Err(SettingsError::StoredLimits);
    }
    let owner = owner.clone();
    tokio::task::spawn_blocking(move || parse_document(&content, &owner))
        .await
        .map_err(|err| task_error(&err))?
        .map_err(|err| if err == SettingsError::Limits { SettingsError::StoredLimits } else { err })
}

pub fn validate_settings_table(table: &str) -> Result<(), SettingsError> {
    if !settings_id_valid(table) {
        return Err(SettingsError::IdInvalid);
    }
    Ok(())
}

pub fn settings_precondition(value: &str) -> Result<&str, SettingsError> {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return Err(SettingsError::PreconditionInvalid);
    };
    if inner.len() != 64 || !inner.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(SettingsError::PreconditionInvalid);
    }
    Ok(value)
}

/// The caller holds the owner lock and rechecks live account existence before mutation.
pub async fn mutate_settings_unlocked(
    path: &Path,
    owner: &SettingsOwner,
    table: &str,
    layout: Option<&TableLayoutPreferencesDto>,
    expected: &str,
    locks: &FileLockManager,
) -> Result<(TableLayoutPreferencesDto, String), SettingsError> {
    validate_settings_table(table)?;
    settings_precondition(expected)?;
    if let Some(layout) = layout {
        layout.validate(true).map_err(|code| match code {
            "settings_limits_exceeded" => SettingsError::Limits,
            "settings_id_invalid" => SettingsError::IdInvalid,
            _ => SettingsError::PayloadInvalid,
        })?;
    }
    let mut entity = load_settings_unlocked(path, owner).await?;
    if entity.etag(owner, table)? != expected {
        return Err(SettingsError::PreconditionFailed);
    }
    entity.update(table, layout)?;
    entity.validate(owner)?;
    let layout = entity.layout(table)?;
    let etag = entity.etag_with_layout(owner, table, &layout)?;
    let result = (layout, etag);
    let content = tokio::task::spawn_blocking(move || serde_saphyr::to_string(&entity.document))
        .await
        .map_err(|err| task_error(&err))?
        .map_err(|err| {
            log::warn!("User settings serialization failed: {err}");
            SettingsError::Io
        })?;
    if content.len() > SETTINGS_MAX_FILE_BYTES {
        return Err(SettingsError::Limits);
    }
    safe_path(path).await?;
    write_file_atomic(path, content.as_bytes()).await.map_err(|err| io_error(&err))?;
    locks.mark_internal_write_content(path, content.as_bytes()).await;
    Ok(result)
}

pub async fn remove_settings_unlocked(path: &Path, _owner: &SettingsOwner) -> Result<(), SettingsError> {
    safe_path(path).await?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_error(&err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn filenames_are_portable_and_unambiguous() -> TestResult {
        assert_eq!(settings_filename("alice")?, "alice.yml");
        assert_ne!(settings_filename("alice")?.to_lowercase(), settings_filename("Alice")?.to_lowercase());
        assert_ne!(settings_filename("a/b")?, settings_filename("a%2Fb")?);
        for name in ["..", "a\\b", "é", "con", "lpt1", "a."] {
            let filename = settings_filename(name)?;
            assert!(!filename.contains('/') && !filename.contains('\\'));
            assert!(!filename.starts_with('.'));
        }
        assert!(settings_filename(&"é".repeat(256))?.len() < 255);
        Ok(())
    }

    #[test]
    fn filename_hash_prefixes_preserve_existing_encoding_boundaries() -> TestResult {
        let ordinary = "a".repeat(180);
        assert_eq!(settings_filename(&ordinary)?, format!("{ordinary}.yml"));
        let escaped = "%".repeat(60);
        assert_eq!(settings_filename(&escaped)?, format!("{}.yml", "%25".repeat(60)));
        let mut cases = vec![
            ("a".repeat(181), "a".repeat(80)),
            ("%".repeat(61), "%25".repeat(26)),
            ("é".repeat(64), "%C3%A9".repeat(13)),
            ("con".into(), "con".into()),
            ("lpt1".into(), "lpt1".into()),
        ];
        for literal_len in 77..=80 {
            let username = format!("{}A{}", "a".repeat(literal_len), "b".repeat(181 - literal_len));
            let prefix = if literal_len == 77 { format!("{}%41", "a".repeat(77)) } else { "a".repeat(literal_len) };
            cases.push((username, prefix));
        }
        for (username, prefix) in cases {
            assert_eq!(
                settings_filename(&username)?,
                format!("{prefix}~{}.yml", blake3::hash(username.as_bytes()).to_hex())
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_failures_are_internal_server_errors() -> TestResult {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let error = task.await.err().ok_or("cancelled task must fail")?;
        assert!(error.is_cancelled());
        let error = task_error(&error);
        assert_eq!(error, SettingsError::Io);
        assert_eq!(error.status(), 500);
        Ok(())
    }

    #[test]
    fn namespaces_preserve_encoded_and_trimmed_subjects() -> TestResult {
        for username in ["Alice", " alice ", "a/b", "a\\b", "a%2Fb", "a\0b", "é"] {
            for (subject, namespace) in [(UserId::web(username), "web"), (UserId::api(username), "api")] {
                let owner = SettingsOwner::authenticated(&subject, username)?;
                assert_eq!(owner.namespace()?, namespace);
                let mut mismatched = owner;
                mismatched.username.push('x');
                assert_eq!(mismatched.namespace(), Err(SettingsError::OwnerMismatch));
            }
        }
        assert_eq!(SettingsOwner::local().namespace()?, "local");
        assert_eq!(SettingsOwner::authenticated(&UserId::builtin_admin(), "Admin")?.namespace()?, "builtin");
        Ok(())
    }

    #[test]
    fn streamed_etags_preserve_byte_encoding_and_field_presence() -> TestResult {
        let owner = SettingsOwner::local();
        let mut entity = UserSettingsEntity::empty(&owner);
        let absent = entity.etag(&owner, "users")?;
        entity.document["preferences"] = json!({"web_ui": {"tables": {
            "users": {"column_order": [], "column_visibility": {}, "widths": {"username": 200}},
            "future.table": {"column_order": ["name"], "future": true}
        }}});
        for table in ["users", "future.table", "playlist.inputs"] {
            let value = entity.tables().and_then(|tables| tables.get(table));
            let presence = ["column_order", "column_visibility"].map(|key| value.is_some_and(|v| v.get(key).is_some()));
            let bytes =
                serde_json::to_vec(&(&owner.subject_id, &owner.username, table, presence, entity.layout(table)?))?;
            assert_eq!(entity.etag(&owner, table)?, format!("\"{}\"", blake3::hash(&bytes).to_hex()));
        }
        assert_ne!(entity.etag(&owner, "users")?, absent);
        let dto = entity.dto(&owner)?;
        assert_eq!(dto.preferences.web_ui.tables.len(), 2);
        assert_eq!(dto.section_etags.len(), 2);
        for table in ["users", "future.table"] {
            assert_eq!(dto.section_etags.get(table), Some(&entity.etag(&owner, table)?));
        }
        assert!(!dto.section_etags.contains_key("playlist.inputs"));
        assert_eq!(dto.preferences.web_ui.tables["future.table"].column_order, ["name"]);
        Ok(())
    }

    #[tokio::test]
    async fn new_tables_respect_storage_limits_without_overwriting_settings() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::local();
        let path = settings_path(dir.path(), &owner)?;
        let locks = FileLockManager::new();
        let _guard = locks.write_lock(&path).await;
        let mut entity = UserSettingsEntity::empty(&owner);
        let layout = TableLayoutPreferencesDto { column_order: vec!["name".into()], ..Default::default() };
        for index in 0..SETTINGS_MAX_TABLES {
            entity.update(&format!("future.table.{index}"), Some(&layout))?;
        }
        entity.validate(&owner)?;
        tokio::fs::create_dir_all(path.parent().ok_or("missing parent")?).await?;
        let content = serde_saphyr::to_string(&entity.document)?;
        tokio::fs::write(&path, &content).await?;
        let table = "future.table.extra";
        let etag = entity.etag(&owner, table)?;
        assert_eq!(
            mutate_settings_unlocked(&path, &owner, table, Some(&layout), &etag, &locks).await,
            Err(SettingsError::Limits)
        );
        assert_eq!(tokio::fs::read_to_string(&path).await?, content);
        let table = "future.table.0";
        let etag = entity.etag(&owner, table)?;
        mutate_settings_unlocked(&path, &owner, table, None, &etag, &locks).await?;
        let entity = load_settings_unlocked(&path, &owner).await?;
        let etag = entity.etag(&owner, "future.table.extra")?;
        mutate_settings_unlocked(&path, &owner, "future.table.extra", Some(&layout), &etag, &locks).await?;
        assert_eq!(load_settings_unlocked(&path, &owner).await?.dto(&owner)?.section_etags.len(), SETTINGS_MAX_TABLES);
        Ok(())
    }

    #[test]
    fn known_storage_fields_remain_strict() {
        let owner = SettingsOwner::local();
        for fields in [
            json!({"column_order": null}),
            json!({"column_order": [1]}),
            json!({"column_visibility": {"name": "false"}}),
        ] {
            let mut entity = UserSettingsEntity::empty(&owner);
            entity.document["preferences"] = json!({"web_ui": {"tables": {"users": fields}}});
            assert_eq!(entity.layout("users"), Err(SettingsError::Invalid));
        }
    }

    #[tokio::test]
    async fn removal_accepts_corrupt_oversized_and_mismatched_documents() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::local();
        let path = settings_path(dir.path(), &owner)?;
        tokio::fs::create_dir_all(path.parent().ok_or("missing parent")?).await?;
        let locks = FileLockManager::new();
        let _guard = locks.write_lock(&path).await;
        for document in [
            b"version: [".to_vec(),
            vec![b' '; SETTINGS_MAX_FILE_BYTES + 1],
            b"version: 2\nowner: {}".to_vec(),
            serde_saphyr::to_string(
                &UserSettingsEntity::empty(&SettingsOwner::authenticated(&UserId::web("other"), "other")?).document,
            )?
            .into_bytes(),
        ] {
            tokio::fs::write(&path, document).await?;
            remove_settings_unlocked(&path, &owner).await?;
            assert!(!path.exists());
        }
        remove_settings_unlocked(&path, &owner).await?;
        tokio::fs::create_dir(&path).await?;
        assert_eq!(remove_settings_unlocked(&path, &owner).await, Err(SettingsError::UnsafeFile));
        Ok(())
    }

    #[tokio::test]
    async fn roundtrip_preserves_sections_and_detects_conflicts() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::authenticated(&UserId::web("alice"), "alice")?;
        let path = settings_path(dir.path(), &owner)?;
        let locks = FileLockManager::new();
        let _guard = locks.write_lock(&path).await;
        let entity = load_settings_unlocked(&path, &owner).await?;
        assert!(!path.exists());
        let first = TableLayoutPreferencesDto { column_order: vec!["username".into()], ..Default::default() };
        let original = entity.etag(&owner, "users")?;
        let (_, users_etag) = mutate_settings_unlocked(&path, &owner, "users", Some(&first), &original, &locks).await?;
        let mut entity = load_settings_unlocked(&path, &owner).await?;
        entity.document["preferences"]["player"] = json!({"future": [1, 2]});
        entity.document["preferences"]["web_ui"]["tables"]["users"]["widths"] = json!({"username": 200});
        tokio::fs::write(&path, serde_saphyr::to_string(&entity.document)?).await?;
        let other_etag = entity.etag(&owner, "playlist.inputs")?;
        mutate_settings_unlocked(&path, &owner, "playlist.inputs", Some(&first), &other_etag, &locks).await?;
        assert_eq!(load_settings_unlocked(&path, &owner).await?.etag(&owner, "users")?, users_etag);
        assert_eq!(
            mutate_settings_unlocked(&path, &owner, "users", Some(&first), &original, &locks).await,
            Err(SettingsError::PreconditionFailed)
        );
        mutate_settings_unlocked(&path, &owner, "users", None, &users_etag, &locks).await?;
        let entity = load_settings_unlocked(&path, &owner).await?;
        assert_eq!(entity.document["preferences"]["player"]["future"], json!([1, 2]));
        assert_eq!(entity.document["preferences"]["web_ui"]["tables"]["users"]["widths"]["username"], 200);
        assert_eq!(entity.layout("playlist.inputs")?, first);
        assert!(locks.is_internal_write_revision(&path).await);
        Ok(())
    }

    #[test]
    fn invalid_documents_and_preconditions_are_rejected() {
        let owner = SettingsOwner::local();
        for yaml in ["version: 2\nowner: {}", "version: 1\nversion: 1", "a: &a []\nb: *a", "a: !custom value"] {
            assert!(parse_document(yaml.as_bytes(), &owner).is_err());
        }
        for token in ["*", "W/\"abc\"", "\"abc\"", ""] {
            assert!(settings_precondition(token).is_err());
        }
    }

    #[tokio::test]
    async fn invalid_files_are_not_overwritten_and_depth_and_size_are_bounded() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::local();
        let path = settings_path(dir.path(), &owner)?;
        let parent = path.parent().ok_or("missing parent")?;
        tokio::fs::create_dir_all(parent).await?;
        let locks = FileLockManager::new();
        let _guard = locks.write_lock(&path).await;
        let empty = UserSettingsEntity::empty(&owner);
        let etag = empty.etag(&owner, "users")?;
        for document in [
            b"version: 2\nowner: {}".to_vec(),
            b"version: 1\nversion: 1".to_vec(),
            vec![b' '; SETTINGS_MAX_FILE_BYTES + 1],
        ] {
            tokio::fs::write(&path, &document).await?;
            assert!(mutate_settings_unlocked(
                &path,
                &owner,
                "users",
                Some(&TableLayoutPreferencesDto::default()),
                &etag,
                &locks
            )
            .await
            .is_err());
            assert_eq!(tokio::fs::read(&path).await?, document);
        }
        let mut document = empty.document;
        document["owner"]["username"] = json!("someone_else");
        assert_eq!(
            parse_document(serde_saphyr::to_string(&document)?.as_bytes(), &owner).err(),
            Some(SettingsError::OwnerMismatch)
        );
        let deep = format!("value: {}0{}", "[".repeat(17), "]".repeat(17));
        assert_eq!(parse_document(deep.as_bytes(), &owner).err(), Some(SettingsError::Limits));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_tables_merge_and_same_section_has_one_winner() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::local();
        let path = settings_path(dir.path(), &owner)?;
        let locks = FileLockManager::new();
        let empty = load_settings_unlocked(&path, &owner).await?;
        let users = empty.etag(&owner, "users")?;
        let inputs = empty.etag(&owner, "playlist.inputs")?;
        let layout = TableLayoutPreferencesDto { column_order: vec!["name".into()], ..Default::default() };
        let save = |table, expected| {
            let path = &path;
            let owner = &owner;
            let locks = &locks;
            let layout = &layout;
            async move {
                let _guard = locks.write_lock(path).await;
                mutate_settings_unlocked(path, owner, table, Some(layout), expected, locks).await
            }
        };
        let (first, second) = tokio::join!(save("users", &users), save("playlist.inputs", &inputs));
        first?;
        second?;
        let current = load_settings_unlocked(&path, &owner).await?;
        assert_eq!(current.layout("users")?, layout);
        assert_eq!(current.layout("playlist.inputs")?, layout);
        let etag = current.etag(&owner, "config.schedules")?;
        let (first, second) = tokio::join!(save("config.schedules", &etag), save("config.schedules", &etag));
        assert!(matches!(
            (&first, &second),
            (Ok(_), Err(SettingsError::PreconditionFailed)) | (Err(SettingsError::PreconditionFailed), Ok(_))
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_settings_files_are_rejected() -> TestResult {
        let dir = tempfile::tempdir()?;
        let owner = SettingsOwner::local();
        let path = settings_path(dir.path(), &owner)?;
        tokio::fs::create_dir_all(path.parent().ok_or("missing parent")?).await?;
        let target = dir.path().join("private.yml");
        tokio::fs::write(&target, serde_saphyr::to_string(&UserSettingsEntity::empty(&owner).document)?).await?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert_eq!(load_settings_unlocked(&path, &owner).await.err(), Some(SettingsError::UnsafeFile));
        assert_eq!(remove_settings_unlocked(&path, &owner).await, Err(SettingsError::UnsafeFile));
        assert!(target.exists());
        Ok(())
    }
}
