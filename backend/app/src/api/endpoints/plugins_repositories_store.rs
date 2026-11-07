//! Reading and writing `plugins.repositories` in `config.yml` for the
//! marketplace API.
//!
//! The same as-written pattern as `plugins_settings_api::persist_entry_in`: the
//! file is read with `${env:VAR}` references unresolved, only the repository
//! list changes, and the result goes through `save_main_config` (backup kept).
//! The runtime config then takes the change with the urls as configured; they
//! are resolved at fetch time (`PluginRepository::resolved_url`). A resolved
//! value is never written back.
//!
//! The helpers validate nothing but identity (duplicate and unknown ids): the
//! caller runs the DTO `Prepare` and `validate_repository` first.

#![cfg(feature = "plugins")]

use crate::model::AppConfig;
use shared::{
    error::TuliproxError,
    model::{PluginRepositoryDto, PluginsConfigDto},
};
use std::sync::Arc;
use tuliprox_core::model::{PluginRepository, PluginsConfig};

/// One edit of the repository list.
#[derive(Debug, Clone)]
pub(in crate::api) enum RepositoryChange {
    /// Append a new source; its id must not exist.
    Add(PluginRepositoryDto),
    /// Replace the source with this id (the dto's id wins when it differs; it
    /// must not collide with another source).
    Replace(String, PluginRepositoryDto),
    /// Drop the source with this id. Installed plugins stay.
    Remove(String),
}

/// Why a repository change was not stored; the API maps `Duplicate` to 409 and
/// `NotFound` to 404.
#[derive(Debug)]
pub(in crate::api) enum RepositoryStoreError {
    /// A source with this id already exists.
    Duplicate(String),
    /// No source has this id.
    NotFound(String),
    /// The file could not be read or written.
    Store(TuliproxError),
}

impl From<TuliproxError> for RepositoryStoreError {
    fn from(err: TuliproxError) -> Self { Self::Store(err) }
}

impl std::fmt::Display for RepositoryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "marketplace source `{id}` already exists"),
            Self::NotFound(id) => write!(f, "marketplace source `{id}` does not exist"),
            Self::Store(err) => err.fmt(f),
        }
    }
}

/// The ids of the same list, to apply a change without touching anything else.
fn apply_to_list(list: &mut Vec<PluginRepositoryDto>, change: &RepositoryChange) -> Result<(), RepositoryStoreError> {
    match change {
        RepositoryChange::Add(dto) => {
            if list.iter().any(|existing| existing.id == dto.id) {
                return Err(RepositoryStoreError::Duplicate(dto.id.clone()));
            }
            list.push(dto.clone());
        }
        RepositoryChange::Replace(id, dto) => {
            let Some(position) = list.iter().position(|existing| existing.id == *id) else {
                return Err(RepositoryStoreError::NotFound(id.clone()));
            };
            if dto.id != *id && list.iter().any(|existing| existing.id == dto.id) {
                return Err(RepositoryStoreError::Duplicate(dto.id.clone()));
            }
            list[position] = dto.clone();
        }
        RepositoryChange::Remove(id) => {
            let before = list.len();
            list.retain(|existing| existing.id != *id);
            if list.len() == before {
                return Err(RepositoryStoreError::NotFound(id.clone()));
            }
        }
    }
    Ok(())
}

/// The runtime repository list with `list` swapped in.
fn replace_runtime(plugins: &mut PluginsConfig, list: &[PluginRepositoryDto]) {
    plugins.repositories = list.iter().map(PluginRepository::from).collect();
}

async fn read_written(app_config: &AppConfig) -> Result<shared::model::ConfigDto, TuliproxError> {
    let read_path = app_config.paths.load().config_file_path.clone();
    tokio::task::spawn_blocking(move || crate::config_loader::read_config_file(&read_path, false, false))
        .await
        .map_err(|err| TuliproxError::Config(format!("Could not read config.yml: {err}")))?
}

/// The `plugins.repositories` list as `config.yml` has it, `${env:VAR}`
/// references unresolved.
///
/// # Errors
/// When `config.yml` cannot be read or parsed.
pub(in crate::api) async fn written_repositories(
    app_config: &AppConfig,
) -> Result<Vec<PluginRepositoryDto>, TuliproxError> {
    Ok(read_written(app_config).await?.plugins.map(|plugins| plugins.repositories).unwrap_or_default())
}

/// Write `change` to `plugins.repositories` in `config.yml` (backup kept, the
/// `plugins:` block created when absent), then update the runtime config and
/// return the runtime `plugins:` section. The caller holds the write lock of
/// `config.yml` (and the install lock first, like the other mutating plugin
/// routes). Nothing is written when the change is refused.
///
/// # Errors
/// `Duplicate` and `NotFound` for an identity problem, `Store` when the file
/// cannot be read or written.
pub(in crate::api) async fn persist_repositories(
    app_config: &AppConfig,
    change: RepositoryChange,
) -> Result<PluginsConfig, RepositoryStoreError> {
    let (file_path, backup_dir) = {
        let paths = app_config.paths.load();
        let config = app_config.config.load();
        (paths.config_file_path.clone(), config.get_backup_dir().to_string())
    };
    let mut dto = read_written(app_config).await?;
    let plugins = dto.plugins.get_or_insert_with(PluginsConfigDto::default);
    apply_to_list(&mut plugins.repositories, &change)?;
    let written = plugins.repositories.clone();
    crate::config_loader::save_main_config(&file_path, &backup_dir, &dto).await?;

    let mut next = app_config.plugins.load_full().map(|current| (*current).clone()).unwrap_or_default();
    replace_runtime(&mut next, &written);
    app_config.plugins.store(Some(Arc::new(next.clone())));
    app_config.config.rcu(|current| {
        let mut config = current.as_ref().clone();
        config.plugins = Some(next.clone());
        Arc::new(config)
    });
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use tempfile::TempDir;

    const BASE: &str = "api:\n  host: ${env:TP_TEST_REPO_HOST}\n  port: 8901\n";

    fn repo(id: &str, url: &str) -> PluginRepositoryDto {
        PluginRepositoryDto { id: id.to_string(), name: format!("{id} name"), url: url.to_string(), enabled: true }
    }

    fn app_config_for(dir: &TempDir, yaml: &str) -> (Arc<AppConfig>, std::path::PathBuf) {
        let file = dir.path().join("config.yml");
        std::fs::write(&file, yaml).unwrap();
        let config =
            Config { backup_dir: Some(dir.path().join("backup").to_string_lossy().to_string()), ..Config::default() };
        let app_config = create_test_app_state(config).app_config.clone();
        let mut paths = app_config.paths.load().as_ref().clone();
        paths.config_file_path = file.to_string_lossy().to_string();
        app_config.paths.store(Arc::new(paths));
        (app_config, file)
    }

    #[tokio::test]
    async fn add_creates_the_plugins_block_and_keeps_references_as_written() {
        let dir = TempDir::new().unwrap();
        let (app_config, file) = app_config_for(&dir, BASE);
        let saved =
            persist_repositories(&app_config, RepositoryChange::Add(repo("community", "${env:TP_TEST_REPO_URL}")))
                .await
                .unwrap();
        assert_eq!(saved.repositories[0].url, "${env:TP_TEST_REPO_URL}");
        assert_eq!(app_config.plugins.load_full().unwrap().repositories.len(), 1);
        assert_eq!(app_config.config.load().plugins.as_ref().unwrap().repositories[0].id, "community");

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("${env:TP_TEST_REPO_HOST}"), "{text}");
        assert!(text.contains("${env:TP_TEST_REPO_URL}"), "{text}");
        assert!(text.contains("plugins:"), "{text}");
        let written = written_repositories(&app_config).await.unwrap();
        assert_eq!(written, vec![repo("community", "${env:TP_TEST_REPO_URL}")]);
    }

    #[tokio::test]
    async fn add_keeps_the_other_repositories_and_plugin_settings() {
        let dir = TempDir::new().unwrap();
        let yaml = format!(
            "{BASE}plugins:\n  enabled: true\n  repositories:\n    - id: first-repo\n      name: First\n      url: https://a.test/m.json\n  plugins:\n    - id: demo-plugin\n      settings:\n        token: ${{env:TP_TEST_SECRET}}\n"
        );
        let (app_config, file) = app_config_for(&dir, &yaml);
        persist_repositories(&app_config, RepositoryChange::Add(repo("second-repo", "https://b.test/m.json")))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("${env:TP_TEST_SECRET}"), "{text}");
        assert!(text.contains("enabled: true"), "{text}");
        let ids: Vec<_> = written_repositories(&app_config).await.unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["first-repo", "second-repo"]);
        assert!(dir.path().join("backup").exists(), "a backup of config.yml is kept");
    }

    #[tokio::test]
    async fn replace_and_remove_change_only_the_named_source() {
        let dir = TempDir::new().unwrap();
        let (app_config, _) = app_config_for(&dir, BASE);
        persist_repositories(&app_config, RepositoryChange::Add(repo("first-repo", "https://a.test/m.json")))
            .await
            .unwrap();
        persist_repositories(&app_config, RepositoryChange::Add(repo("second-repo", "https://b.test/m.json")))
            .await
            .unwrap();

        let mut changed = repo("first-repo", "${env:TP_TEST_REPO_URL}");
        changed.enabled = false;
        let saved =
            persist_repositories(&app_config, RepositoryChange::Replace("first-repo".to_string(), changed.clone()))
                .await
                .unwrap();
        assert_eq!(PluginRepositoryDto::from(&saved.repositories[0]), changed);
        assert_eq!(written_repositories(&app_config).await.unwrap()[0], changed);

        let saved =
            persist_repositories(&app_config, RepositoryChange::Remove("first-repo".to_string())).await.unwrap();
        assert_eq!(saved.repositories.len(), 1);
        assert_eq!(
            written_repositories(&app_config).await.unwrap(),
            vec![repo("second-repo", "https://b.test/m.json")]
        );
    }

    #[tokio::test]
    async fn identity_problems_are_typed_and_write_nothing() {
        let dir = TempDir::new().unwrap();
        let (app_config, file) = app_config_for(&dir, BASE);
        persist_repositories(&app_config, RepositoryChange::Add(repo("first-repo", "https://a.test/m.json")))
            .await
            .unwrap();
        persist_repositories(&app_config, RepositoryChange::Add(repo("second-repo", "https://b.test/m.json")))
            .await
            .unwrap();
        let before = std::fs::read_to_string(&file).unwrap();

        let duplicate =
            persist_repositories(&app_config, RepositoryChange::Add(repo("first-repo", "https://c.test/m.json"))).await;
        assert!(matches!(duplicate, Err(RepositoryStoreError::Duplicate(id)) if id == "first-repo"));
        let rename_onto_other = persist_repositories(
            &app_config,
            RepositoryChange::Replace("first-repo".to_string(), repo("second-repo", "https://c.test/m.json")),
        )
        .await;
        assert!(matches!(rename_onto_other, Err(RepositoryStoreError::Duplicate(_))));
        let unknown_replace = persist_repositories(
            &app_config,
            RepositoryChange::Replace("nope-repo".to_string(), repo("nope-repo", "https://c.test/m.json")),
        )
        .await;
        assert!(matches!(unknown_replace, Err(RepositoryStoreError::NotFound(id)) if id == "nope-repo"));
        let unknown_remove = persist_repositories(&app_config, RepositoryChange::Remove("nope-repo".to_string())).await;
        assert!(matches!(unknown_remove, Err(RepositoryStoreError::NotFound(_))));

        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert_eq!(app_config.plugins.load_full().unwrap().repositories.len(), 2);
    }
}
