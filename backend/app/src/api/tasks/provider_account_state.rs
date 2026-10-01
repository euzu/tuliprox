use crate::{
    api::{
        internal_csv::csv_patch_batch_update_account_states,
        model::AppState,
        source_yml_patch::{apply_runtime_source_patches, execute_source_yml_patches_locked, SourcesYmlPatch},
    },
    model::{ConfigInput, SourcesConfig},
    repository::{get_csv_file_path, BatchAccountStateUpdate},
};
use shared::{
    error::TuliproxError,
    model::{EventMessage, InputType, ProviderAccountEvent, ProviderAccountState},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::model::ProviderAccountObservation;

/// Accounts expiring within this window trigger an "expiring" notification.
const EXPIRING_WARNING_WINDOW_SECS: i64 = 3 * 24 * 60 * 60;

/// Result of one persistence pass.
#[derive(Default)]
pub(super) struct PersistReport {
    /// Accounts whose observation is stored in the source files.
    pub persisted: Vec<Arc<str>>,
    /// Accounts that could not be stored, with the reason.
    pub failed: Vec<(Arc<str>, String)>,
    pub first_error: Option<TuliproxError>,
}

impl PersistReport {
    fn fail(&mut self, names: impl IntoIterator<Item = Arc<str>>, err: TuliproxError) {
        let message = err.to_string();
        self.failed.extend(names.into_iter().map(|name| (name, message.clone())));
        self.first_error.get_or_insert(err);
    }
}

pub(super) async fn persist_provider_account_observations(app_state: &Arc<AppState>) {
    let pending = app_state.active_provider.pending_account_observations();
    if pending.is_empty() {
        return;
    }
    let report = persist_observations(app_state, &pending).await;
    for (name, err) in report.failed {
        log::warn!("Could not persist provider account {name}: {err}");
        let event = crate::model::NotificationEvent::new(
            shared::model::notification::EventId::new("provider.account.persistence_failed"),
            "Provider account update could not be saved",
            format!("Account {name} remains excluded when blocked; saving will be retried. {err}"),
        )
        .with_severity(shared::model::notification::Severity::Error)
        .with_dedup_key(format!("provider-account-persistence:{name}"));
        let client = app_state.http_client.load();
        tuliprox_messaging::send_event(&app_state.app_config, &client, event).await;
    }
}

/// Persists a single observation; see [`persist_observations`].
#[cfg(test)]
pub(super) async fn persist_observation(
    app_state: &Arc<AppState>,
    observation: &ProviderAccountObservation,
) -> Result<bool, TuliproxError> {
    let mut report = persist_observations(app_state, std::slice::from_ref(observation)).await;
    match report.first_error.take() {
        Some(err) => Err(err),
        None => Ok(!report.persisted.is_empty()),
    }
}

/// Persists observations grouped by their source file, so each file is locked, backed up
/// and written once per pass and the provider lineup is rebuilt once per file.
pub(super) async fn persist_observations(
    app_state: &Arc<AppState>,
    observations: &[ProviderAccountObservation],
) -> PersistReport {
    let mut report = PersistReport::default();
    let sources = app_state.app_config.sources.load_full();
    let mut groups: Vec<(PathBuf, Option<String>, Vec<&ProviderAccountObservation>)> = Vec::new();
    for observation in observations {
        let Some(owner) = find_owner(&sources, &observation.name) else {
            app_state.active_provider.acknowledge_account_observation(observation);
            continue;
        };
        let path = match &owner.t_batch_url {
            Some(batch_url) => match get_csv_file_path(batch_url) {
                Ok(path) => path,
                Err(err) => {
                    report.fail([Arc::clone(&observation.name)], TuliproxError::ConfigInput(err.to_string()));
                    continue;
                }
            },
            None => PathBuf::from(&app_state.app_config.paths.load().sources_file_path),
        };
        match groups.iter_mut().find(|(group_path, _, _)| *group_path == path) {
            Some((_, _, group)) => group.push(observation),
            None => groups.push((path, owner.t_batch_url.clone(), vec![observation])),
        }
    }
    for (path, batch, group) in groups {
        persist_group(app_state, &path, batch.as_deref(), &group, &mut report).await;
    }
    report
}

fn find_owner<'a>(sources: &'a SourcesConfig, name: &str) -> Option<&'a ConfigInput> {
    sources
        .inputs
        .iter()
        .find(|input| {
            input.name.as_ref() == name || input.aliases.iter().flatten().any(|alias| alias.name.as_ref() == name)
        })
        .map(AsRef::as_ref)
}

struct ResolvedAccount<'a> {
    input: &'a ConfigInput,
    url: &'a str,
    username: Option<&'a str>,
    password: Option<&'a str>,
    exp_date: Option<i64>,
    disabled: bool,
}

/// Looks the account up again under the file lock; `None` when it vanished, moved or changed credentials.
fn resolve_account<'a>(
    sources: &'a SourcesConfig,
    observation: &ProviderAccountObservation,
    batch: Option<&str>,
) -> Option<ResolvedAccount<'a>> {
    let input = find_owner(sources, &observation.name)?;
    let account = if input.name == observation.name {
        ResolvedAccount {
            input,
            url: &input.url,
            username: input.username.as_deref(),
            password: input.password.as_deref(),
            exp_date: input.exp_date,
            disabled: input.account_disabled,
        }
    } else {
        let alias = input.aliases.iter().flatten().find(|alias| alias.name == observation.name)?;
        ResolvedAccount {
            input,
            url: &alias.url,
            username: alias.username.as_deref(),
            password: alias.password.as_deref(),
            exp_date: alias.exp_date,
            disabled: !alias.enabled,
        }
    };
    let identity = shared::model::ProviderAccountIdentity::new(account.url, account.username, account.password);
    (input.t_batch_url.as_deref() == batch && identity == observation.identity).then_some(account)
}

async fn persist_group(
    app_state: &Arc<AppState>,
    path: &Path,
    batch: Option<&str>,
    group: &[&ProviderAccountObservation],
    report: &mut PersistReport,
) {
    let guard = app_state.app_config.file_locks.write_lock(path).await;
    let sources = app_state.app_config.sources.load_full();
    let mut patches = Vec::with_capacity(group.len());
    let mut csv_updates = Vec::new();
    let mut accepted = Vec::with_capacity(group.len());
    for &observation in group {
        let Some(account) = resolve_account(&sources, observation, batch) else {
            app_state.active_provider.acknowledge_account_observation(observation);
            continue;
        };
        // A renewal under this lock clears the pending entry; a stale ban must not be written back.
        if !app_state.active_provider.is_observation_pending(observation) {
            continue;
        }
        notify_account_transition(app_state, observation, account.username, account.exp_date, account.disabled);
        patches.push(SourcesYmlPatch::SetObservedAccount {
            input_name: Arc::clone(&account.input.name),
            observation: observation.clone(),
        });
        if batch.is_some() {
            csv_updates.push(BatchAccountStateUpdate {
                identity: observation.identity,
                account_name: Arc::clone(&observation.name),
                username: account.username.map(ToString::to_string),
                password: account.password.map(ToString::to_string),
                url: Some(account.url.to_string()),
                exp_date: observation.exp_date,
                disable: observation.is_persistent_block(),
            });
        }
        accepted.push(observation);
    }
    if accepted.is_empty() {
        return;
    }
    let names = || accepted.iter().map(|observation| Arc::clone(&observation.name));
    // Runtime exclusions survive failed writes. Each CAS also checks the identity.
    if let Err(err) = apply_runtime_source_patches(&app_state.app_config, &patches) {
        report.fail(names(), err);
        return;
    }
    // `Some(matched)` for CSV; the CSV mutation already rebuilds the lineup when it changes the file.
    let (written, refreshed) = if batch.is_some() {
        match csv_patch_batch_update_account_states(app_state, &guard, InputType::XtreamBatch, path, &csv_updates).await
        {
            Ok(receipt) => (Ok(Some(receipt.matched)), receipt.changed),
            Err(err) => (Err(err), false),
        }
    } else {
        (execute_source_yml_patches_locked(&app_state.app_config, path, &patches, &guard).await.map(|_| None), false)
    };
    if !refreshed {
        app_state.active_provider.update_config(&app_state.app_config);
    }
    match written {
        Err(err) => report.fail(names(), err),
        Ok(matched) => {
            for observation in accepted {
                if matched.as_ref().is_some_and(|matched| !matched.contains(&observation.identity)) {
                    report.fail(
                        [Arc::clone(&observation.name)],
                        TuliproxError::ConfigInput(format!(
                            "Account {} is missing or changed in alias CSV",
                            observation.name
                        )),
                    );
                } else {
                    app_state.active_provider.acknowledge_account_observation(observation);
                    report.persisted.push(Arc::clone(&observation.name));
                }
            }
        }
    }
}

fn notify_account_transition(
    app_state: &AppState,
    observation: &ProviderAccountObservation,
    username: Option<&str>,
    previous_expiry: Option<i64>,
    was_disabled: bool,
) {
    let newly_blocked = observation.is_blocked() && !was_disabled;
    let expiry_changed = observation.exp_date.is_some() && observation.exp_date != previous_expiry;
    if !newly_blocked && !expiry_changed {
        return;
    }
    let expired = crate::model::is_input_expired(observation.exp_date);
    let expiring = observation
        .exp_date
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp() + EXPIRING_WARNING_WINDOW_SECS);
    if !observation.is_blocked() && !expiring {
        return;
    }
    let (state, summary) = if expired {
        (ProviderAccountState::Expired, "expired")
    } else if observation.is_persistent_block() {
        (ProviderAccountState::StatusChanged, "blocked")
    } else if observation.is_blocked() {
        (ProviderAccountState::StatusChanged, "temporarily unavailable")
    } else {
        (ProviderAccountState::Expiring, "expiring")
    };
    let recovery = if observation.is_persistent_block() {
        "; it stays excluded until it is renewed, its credentials change, or `account_disabled`/`enabled` is reset in the source file"
    } else {
        ""
    };
    app_state.event_manager.send_event(EventMessage::ProviderAccount(ProviderAccountEvent {
        state,
        username: username.unwrap_or_default().to_string(),
        provider: observation.name.to_string(),
        status: observation.status.map(|status| status.to_string()),
        expires_at: observation.exp_date,
        message: format!("Provider account {}: {summary}{recovery}", observation.name),
    }));
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::model::{Config, ConfigInput, SourcesConfig};
    use shared::model::ProxyUserStatus;

    pub(in crate::api::tasks) async fn fixture(
        batch: bool,
    ) -> Result<(tempfile::TempDir, Arc<AppState>, std::path::PathBuf), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(if batch { "aliases.csv" } else { "source.yml" });
        let content = if batch {
            "#name;username;password;url;enabled;exp_date\ncsv-root;user;pass;http://provider;1;\n"
        } else {
            "inputs:\n  - name: root\n    type: xtream\n    url: http://provider\n    username: user\n    password: pass\nsources: []\n"
        };
        tokio::fs::write(&path, content).await?;
        let app = crate::api::model::create_test_app_state(Config {
            backup_dir: Some(dir.path().join("backups").to_string_lossy().into_owned()),
            ..Config::default()
        });
        let input = ConfigInput {
            id: 1,
            name: Arc::from("root"),
            input_type: InputType::Xtream,
            enabled: true,
            url: "http://provider".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            t_batch_url: batch.then(|| path.to_string_lossy().into_owned()),
            ..ConfigInput::default()
        };
        app.app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input)], ..SourcesConfig::default() }));
        app.app_config.paths.rcu(|paths| {
            let mut next = (**paths).clone();
            next.sources_file_path = path.to_string_lossy().into_owned();
            next
        });
        app.app_config.file_locks.mark_internal_write_content(&path, content.as_bytes()).await;
        app.active_provider.update_config(&app.app_config);
        Ok((dir, app, path))
    }

    fn banned() -> ProviderAccountObservation {
        ProviderAccountObservation {
            name: Arc::from("root"),
            identity: shared::model::ProviderAccountIdentity::new("http://provider", Some("user"), Some("pass")),
            status: Some(ProxyUserStatus::Banned),
            exp_date: None,
        }
    }

    const CSV_WITH_ALIAS: &str = "#name;username;password;url;enabled;exp_date\n\
csv-root;user;pass;http://provider;1;\n\
alias;user2;pass2;http://provider;1;\n";

    const YAML_WITH_ALIAS: &str = "inputs:\n  - name: root\n    type: xtream\n    url: http://provider\n    username: user\n    password: pass\n    aliases:\n      - name: alias\n        url: http://provider\n        username: user2\n        password: pass2\nsources: []\n";

    /// Writes `content` as the synchronized file state and adds the matching runtime alias.
    async fn with_alias(app: &Arc<AppState>, path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        tokio::fs::write(path, content).await?;
        app.app_config.file_locks.mark_internal_write_content(path, content.as_bytes()).await;
        app.app_config.sources.rcu(|sources| {
            let mut next = (**sources).clone();
            if let Some(input) = next.inputs.first_mut() {
                Arc::make_mut(input).aliases = Some(vec![crate::model::ConfigInputAlias {
                    id: 2,
                    name: Arc::from("alias"),
                    url: "http://provider".to_string(),
                    username: Some("user2".to_string()),
                    password: Some("pass2".to_string()),
                    priority: 0,
                    max_connections: 0,
                    exp_date: None,
                    enabled: true,
                    stalker: None,
                }]);
            }
            next
        });
        app.active_provider.update_config(&app.app_config);
        Ok(())
    }

    fn banned_alias() -> ProviderAccountObservation {
        ProviderAccountObservation {
            name: Arc::from("alias"),
            identity: shared::model::ProviderAccountIdentity::new("http://provider", Some("user2"), Some("pass2")),
            status: Some(ProxyUserStatus::Banned),
            exp_date: None,
        }
    }

    /// Whether every account in the source file is stored as usable, as a restart would load it.
    async fn stored_accounts_enabled(path: &Path, batch: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if batch {
            let (_, aliases) =
                crate::repository::csv_read_inputs(InputType::XtreamBatch, path.to_string_lossy().as_ref()).await?;
            Ok(aliases.iter().all(|alias| alias.enabled))
        } else {
            let dto: shared::model::SourcesConfigDto = serde_saphyr::from_str(&tokio::fs::read_to_string(path).await?)?;
            Ok(dto
                .inputs
                .iter()
                .all(|input| !input.account_disabled && input.aliases.iter().flatten().all(|alias| alias.enabled)))
        }
    }

    fn runtime_alias_enabled(app: &AppState) -> bool {
        app.app_config.sources.load().inputs[0].aliases.iter().flatten().any(|alias| alias.enabled)
    }

    /// Panel renewal through the production entry point for the given file type.
    async fn renew(
        app: &Arc<AppState>,
        path: &Path,
        batch: bool,
        account: &str,
        credentials: (&str, &str),
    ) -> Result<(), TuliproxError> {
        if batch {
            let guard = app.app_config.file_locks.write_lock(path).await;
            crate::api::internal_csv::csv_patch_batch_update_exp_date(
                app,
                &guard,
                InputType::XtreamBatch,
                path,
                &Arc::from(account),
                credentials.0,
                credentials.1,
                2_000_000_000,
            )
            .await
        } else {
            crate::api::source_yml_patch::execute_account_source_patches(
                app,
                path,
                &[SourcesYmlPatch::UpdatePanelAccountExpiry {
                    input_name: Arc::from("root"),
                    account_name: Arc::from(account),
                    exp_date: 2_000_000_000,
                }],
            )
            .await
            .map(|_| ())
        }
    }

    #[tokio::test]
    async fn queued_ban_does_not_override_same_credential_renewal() -> Result<(), Box<dyn std::error::Error>> {
        for batch in [false, true] {
            let (_dir, app, path) = fixture(batch).await?;
            let observation = banned();
            app.active_provider.observe_account(observation.clone()).ok_or("observation not queued")?;
            // The renewal takes the file lock first and holds it across its file I/O ...
            let mut renewal = Box::pin(renew(&app, &path, batch, "root", ("user", "pass")));
            assert!(futures::poll!(&mut renewal).is_pending());
            // ... so the queued ban waits for the lock and must notice that it was cleared.
            let mut waiting = Box::pin(persist_observation(&app, &observation));
            assert!(futures::poll!(&mut waiting).is_pending());
            renewal.await?;
            assert!(!waiting.await?, "stale ban was persisted (batch={batch})");
            assert!(!app.app_config.sources.load().inputs[0].account_disabled);
            assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
            assert!(stored_accounts_enabled(&path, batch).await?, "batch={batch}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn renewal_lifts_persisted_root_ban() -> Result<(), Box<dyn std::error::Error>> {
        for batch in [false, true] {
            let (_dir, app, path) = fixture(batch).await?;
            let observation = banned();
            app.active_provider.observe_account(observation.clone()).ok_or("observation not queued")?;
            assert!(persist_observation(&app, &observation).await?);
            assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_none());
            let account = if batch { "csv-root" } else { "root" };
            renew(&app, &path, batch, account, ("user", "pass")).await?;
            assert!(!app.app_config.sources.load().inputs[0].account_disabled, "batch={batch}");
            assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some(), "batch={batch}");
            assert!(stored_accounts_enabled(&path, batch).await?, "batch={batch}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn renewal_lifts_persisted_alias_ban() -> Result<(), Box<dyn std::error::Error>> {
        for batch in [false, true] {
            let (_dir, app, path) = fixture(batch).await?;
            with_alias(&app, &path, if batch { CSV_WITH_ALIAS } else { YAML_WITH_ALIAS }).await?;
            let observation = banned_alias();
            app.active_provider.observe_account(observation.clone()).ok_or("observation not queued")?;
            assert!(persist_observation(&app, &observation).await?);
            assert!(!runtime_alias_enabled(&app));
            renew(&app, &path, batch, "alias", ("user2", "pass2")).await?;
            assert!(runtime_alias_enabled(&app), "batch={batch}");
            assert!(!app.active_provider.is_observation_pending(&observation));
            // The file reflects the renewal too, so a restart keeps the alias available.
            assert!(stored_accounts_enabled(&path, batch).await?, "batch={batch}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn login_response_keeps_identity_from_before_the_request() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, app, _path) = fixture(false).await?;
        let input = app.app_config.sources.load().inputs[0].clone();
        let source = crate::model::InputSource::from(input.as_ref());
        let (name, identity) =
            crate::iptv::xtream::login_account_identity(&app.app_config, &source, "user").ok_or("no account")?;
        // The provider URL changes while the login request is in flight.
        app.app_config.sources.rcu(|sources| {
            let mut next = (**sources).clone();
            if let Some(input) = next.inputs.first_mut() {
                Arc::make_mut(input).url = "http://new-provider".to_string();
            }
            next
        });
        app.active_provider.update_config(&app.app_config);
        let stale =
            ProviderAccountObservation { name, identity, status: Some(ProxyUserStatus::Banned), exp_date: None };
        assert!(app.active_provider.observe_account(stale).is_none());
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
        Ok(())
    }

    #[tokio::test]
    async fn pending_status_is_excluded_at_runtime_but_not_persisted() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, app, path) = fixture(false).await?;
        let pending = ProviderAccountObservation { status: Some(ProxyUserStatus::Pending), ..banned() };
        let queued = app.active_provider.observe_account(pending).ok_or("observation not queued")?;
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_none());
        persist_observation(&app, &queued).await?;
        assert!(!tokio::fs::read_to_string(&path).await?.contains("account_disabled"));
        assert!(!app.app_config.sources.load().inputs[0].account_disabled);
        app.active_provider
            .observe_account(ProviderAccountObservation { status: Some(ProxyUserStatus::Active), ..banned() });
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
        Ok(())
    }

    #[tokio::test]
    async fn observations_for_one_csv_are_written_once() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, app, path) = fixture(true).await?;
        with_alias(&app, &path, CSV_WITH_ALIAS).await?;
        let observations = [banned(), banned_alias()];
        for observation in &observations {
            app.active_provider.observe_account(observation.clone()).ok_or("observation not queued")?;
        }
        let report = persist_observations(&app, &observations).await;
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.persisted.len(), 2);
        assert_eq!(std::fs::read_dir(dir.path().join("backups"))?.count(), 1);
        let (_, aliases) =
            crate::repository::csv_read_inputs(InputType::XtreamBatch, path.to_string_lossy().as_ref()).await?;
        assert!(aliases.iter().all(|alias| !alias.enabled));
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_none());
        assert!(app.active_provider.pending_account_observations().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_response_persists_ban_before_restart() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, app, path) = fixture(false).await?;
        app.active_provider.observe_account(banned());
        app.active_provider.observe_account(ProviderAccountObservation {
            status: None,
            exp_date: Some(i64::MAX),
            ..banned()
        });
        persist_provider_account_observations(&app).await;
        let dto: shared::model::SourcesConfigDto = serde_saphyr::from_str(&tokio::fs::read_to_string(&path).await?)?;
        assert!(dto.inputs[0].account_disabled);
        let config = ConfigInput::from(&dto.inputs[0]);
        app.app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(config)], ..SourcesConfig::default() }));
        let restarted = crate::api::model::ActiveProviderManager::new(&app.app_config, &app.event_manager);
        assert!(restarted.get_next_provider(&Arc::from("root")).is_none());
        assert!(app.active_provider.pending_account_observations().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn failed_ban_write_survives_noop_csv_sort_and_explicit_renewal() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, app, path) = fixture(true).await?;
        let blocked = dir.path().join("blocked");
        tokio::fs::write(&blocked, "not a directory").await?;
        app.app_config.config.rcu(|config| {
            let mut next = (**config).clone();
            next.backup_dir = Some(blocked.to_string_lossy().into_owned());
            next
        });
        let observation = banned();
        app.active_provider.observe_account(observation.clone());
        assert!(persist_observation(&app, &observation).await.is_err());
        app.app_config.config.rcu(|config| {
            let mut next = (**config).clone();
            next.backup_dir = Some(dir.path().join("backups").to_string_lossy().into_owned());
            next
        });
        let guard = app.app_config.file_locks.write_lock(&path).await;
        assert!(
            !crate::api::internal_csv::csv_patch_batch_sort_by_exp_date(
                &app,
                &guard,
                InputType::XtreamBatch,
                &path,
                crate::repository::AliasExpDateSortOrder::NewestFirst
            )
            .await?
        );
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_none());
        assert_eq!(app.active_provider.pending_account_observations().len(), 1);
        crate::api::internal_csv::csv_patch_batch_update_exp_date(
            &app,
            &guard,
            InputType::XtreamBatch,
            &path,
            &Arc::from("root"),
            "user",
            "pass",
            2_000_000_000,
        )
        .await?;
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
        assert!(app.active_provider.pending_account_observations().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn queued_ban_does_not_disable_renewed_credentials() -> Result<(), Box<dyn std::error::Error>> {
        for batch in [false, true] {
            let (_dir, app, path) = fixture(batch).await?;
            let observation = banned();
            app.active_provider.observe_account(observation.clone());
            let guard = app.app_config.file_locks.write_lock(&path).await;
            let mut waiting = Box::pin(persist_observation(&app, &observation));
            assert!(futures::poll!(&mut waiting).is_pending());
            if batch {
                crate::api::internal_csv::csv_patch_batch_update_credentials(
                    &app,
                    &guard,
                    InputType::XtreamBatch,
                    &path,
                    &Arc::from("root"),
                    "user",
                    "pass",
                    "user",
                    "renewed",
                    Some(2_000_000_000),
                )
                .await?;
            } else {
                execute_source_yml_patches_locked(
                    &app.app_config,
                    &path,
                    &[SourcesYmlPatch::UpdateRootCredentials {
                        input_name: Arc::from("root"),
                        username: "user".to_string(),
                        password: "renewed".to_string(),
                        exp_date: Some(2_000_000_000),
                    }],
                    &guard,
                )
                .await?;
                app.active_provider.update_config(&app.app_config);
            }
            drop(guard);
            assert!(!waiting.await?);
            let sources = app.app_config.sources.load();
            assert_eq!(sources.inputs[0].password.as_deref(), Some("renewed"));
            assert!(!sources.inputs[0].account_disabled);
            assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
        }
        Ok(())
    }

    #[tokio::test]
    async fn fetched_renewal_unblocks_root_expired_at_load() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, app, _path) = fixture(false).await?;
        app.app_config.sources.rcu(|sources| {
            let mut next = (**sources).clone();
            if let Some(input) = next.inputs.first_mut() {
                Arc::make_mut(input).exp_date = Some(1);
            }
            next
        });
        app.active_provider.update_config(&app.app_config);
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_none());
        let renewed = ProviderAccountObservation { status: None, exp_date: Some(i64::MAX), ..banned() };
        let queued = app.active_provider.observe_account(renewed).ok_or("observation not queued")?;
        assert!(persist_observation(&app, &queued).await?);
        let sources = app.app_config.sources.load();
        assert_eq!(sources.inputs[0].exp_date, Some(i64::MAX));
        assert!(!sources.inputs[0].account_disabled);
        assert!(app.active_provider.get_next_provider(&Arc::from("root")).is_some());
        Ok(())
    }

    #[tokio::test]
    async fn external_yaml_edit_is_not_marked_as_internal() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, app, path) = fixture(false).await?;
        let external = tokio::fs::read_to_string(&path)
            .await?
            .replace("    password: pass", "    password: pass\n    persist: external");
        tokio::fs::write(&path, external).await?;
        execute_source_yml_patches_locked(
            &app.app_config,
            &path,
            &[SourcesYmlPatch::SetAccountDisabled { input_name: Arc::from("root"), account_name: Arc::from("root") }],
            &app.app_config.file_locks.write_lock(&path).await,
        )
        .await?;
        assert!(tokio::fs::read_to_string(&path).await?.contains("persist: external"));
        assert!(!app.app_config.file_locks.is_internal_write_revision(&path).await);
        Ok(())
    }
}
