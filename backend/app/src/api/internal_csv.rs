use crate::{
    api::{
        model::AppState,
        source_yml_patch::{merge_runtime_document, runtime_patch_document},
    },
    repository::{self, AliasExpDateSortOrder},
};
use shared::{
    error::TuliproxError,
    model::{ConfigInputAliasDto, InputType},
};
use std::{future::Future, path::Path, sync::Arc};

/// Synchronizes runtime sources with a CSV mutation. Callers refresh the provider manager afterwards.
fn sync_runtime_csv_accounts(
    app_state: &AppState,
    csv_path: &Path,
    before: &[ConfigInputAliasDto],
    after: &[ConfigInputAliasDto],
    renewed_account: Option<&str>,
) -> Result<(), TuliproxError> {
    loop {
        let current = app_state.app_config.sources.load_full();
        let mut doc = runtime_patch_document(&current);
        for (runtime, input) in current.inputs.iter().zip(&mut doc.inputs) {
            if runtime
                .t_batch_url
                .as_ref()
                .and_then(|url| repository::get_csv_file_path(url).ok())
                .is_none_or(|path| path != csv_path)
            {
                continue;
            }
            let root = before.iter().find(|alias| {
                alias.username == runtime.username && alias.password == runtime.password && alias.url == runtime.url
            });
            let root_name = root.map(|alias| &alias.name);
            if let Some(updated) = root_name.and_then(|name| after.iter().find(|alias| &alias.name == name)) {
                input.url.clone_from(&updated.url);
                input.username.clone_from(&updated.username);
                input.password.clone_from(&updated.password);
                input.exp_date = updated.exp_date;
                input.max_connections = updated.max_connections;
                input.priority = updated.priority;
                let renewed =
                    renewed_account.is_some_and(|name| name == input.name.as_ref() || name == updated.name.as_ref());
                input.account_disabled = !updated.enabled || (runtime.account_disabled && !renewed);
            } else if root.is_some() {
                input.account_disabled = true;
            }
            input.aliases = Some(after.iter().filter(|alias| Some(&alias.name) != root_name).cloned().collect());
            for alias in input.aliases.iter_mut().flatten() {
                if renewed_account != Some(alias.name.as_ref())
                    && runtime.aliases.iter().flatten().any(|old| old.name == alias.name && !old.enabled)
                {
                    alias.enabled = false;
                }
            }
        }
        let next = merge_runtime_document(&current, doc)?;
        let previous = app_state.app_config.sources.compare_and_swap(&current, Arc::new(next));
        if Arc::ptr_eq(&previous, &current) {
            if let Some(name) = renewed_account {
                app_state.active_provider.reset_account_health(&Arc::from(name));
            }
            return Ok(());
        }
    }
}

fn backup_dir(app_state: &AppState) -> String { app_state.app_config.config.load().get_backup_dir().into_owned() }

fn parse_csv(input_type: InputType, content: &[u8]) -> Result<Vec<ConfigInputAliasDto>, TuliproxError> {
    repository::csv_read_inputs_from_reader(
        input_type,
        crate::utils::EnvResolvingReader::new(std::io::BufReader::new(std::io::Cursor::new(content))),
    )
    .map_err(|err| TuliproxError::ConfigInput(err.to_string()))
}

/// Runs a repository CSV mutation under the caller's write lock and mirrors the result into the
/// runtime sources. Backups are taken by the repository only when the file actually changes.
async fn mutate_csv<T, F: Future<Output = Result<T, TuliproxError>>>(
    app_state: &AppState,
    _guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
    mutation: F,
    renewed_account: Option<&str>,
) -> Result<T, TuliproxError> {
    let original = tokio::fs::read(csv_path).await.map_err(|err| TuliproxError::Io(err.to_string()))?;
    let synchronized =
        app_state.app_config.file_locks.is_internal_revision_hash(csv_path, &blake3::hash(&original)).await;
    let result = mutation.await?;
    let content = tokio::fs::read(csv_path).await.map_err(|err| TuliproxError::Io(err.to_string()))?;
    if original != content || renewed_account.is_some() {
        let before = parse_csv(input_type, &original)?;
        let after = parse_csv(input_type, &content)?;
        sync_runtime_csv_accounts(app_state, csv_path, &before, &after, renewed_account)?;
        app_state.active_provider.update_config(&app_state.app_config);
    }
    if synchronized {
        app_state.app_config.file_locks.mark_internal_write_content(csv_path, &content).await;
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn csv_patch_batch_append(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    csv_path: &Path,
    input_type: InputType,
    alias_name: &str,
    base_url: &str,
    username: &str,
    password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let backup_dir = backup_dir(app_state);
    mutate_csv(
        app_state,
        guard,
        input_type,
        csv_path,
        repository::csv_patch_batch_append(
            csv_path,
            input_type,
            alias_name,
            base_url,
            username,
            password,
            exp_date,
            &backup_dir,
        ),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn csv_patch_batch_update_exp_date(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
    account_name: &Arc<str>,
    username: &str,
    password: &str,
    exp_date: i64,
) -> Result<(), TuliproxError> {
    let backup_dir = backup_dir(app_state);
    mutate_csv(
        app_state,
        guard,
        input_type,
        csv_path,
        repository::csv_patch_batch_update_exp_date(
            input_type,
            csv_path,
            account_name,
            username,
            password,
            exp_date,
            &backup_dir,
        ),
        Some(account_name.as_ref()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn csv_patch_batch_update_credentials(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
    account_name: &Arc<str>,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
    exp_date: Option<i64>,
) -> Result<(), TuliproxError> {
    let backup_dir = backup_dir(app_state);
    mutate_csv(
        app_state,
        guard,
        input_type,
        csv_path,
        repository::csv_patch_batch_update_credentials(
            input_type,
            csv_path,
            account_name,
            old_username,
            old_password,
            new_username,
            new_password,
            exp_date,
            &backup_dir,
        ),
        Some(account_name.as_ref()),
    )
    .await
}

pub(crate) async fn csv_patch_batch_update_account_states(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
    updates: &[repository::BatchAccountStateUpdate],
) -> Result<repository::CsvAccountPatchResult, TuliproxError> {
    let backup_dir = backup_dir(app_state);
    mutate_csv(
        app_state,
        guard,
        input_type,
        csv_path,
        repository::csv_patch_batch_update_account_states(input_type, csv_path, updates, &backup_dir),
        None,
    )
    .await
}

pub(crate) async fn csv_patch_batch_remove_expired(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
) -> Result<bool, TuliproxError> {
    let backup_dir = backup_dir(app_state);
    let mutation = repository::csv_patch_batch_remove_expired(input_type, csv_path, &backup_dir);
    mutate_csv(app_state, guard, input_type, csv_path, mutation, None).await
}

pub(crate) async fn csv_patch_batch_sort_by_exp_date(
    app_state: &AppState,
    guard: &crate::utils::FileWriteGuard,
    input_type: InputType,
    csv_path: &Path,
    order: AliasExpDateSortOrder,
) -> Result<bool, TuliproxError> {
    let backup_dir = backup_dir(app_state);
    let mutation = repository::csv_patch_batch_sort_by_exp_date(input_type, csv_path, order, &backup_dir);
    mutate_csv(app_state, guard, input_type, csv_path, mutation, None).await
}
