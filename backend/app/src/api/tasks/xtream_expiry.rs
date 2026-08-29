use crate::{
    api::{config_file::ConfigFile, model::AppState},
    config_loader::{persist_source_config_preserving_templates, read_sources_file_from_path},
    iptv::xtream::get_xtream_stream_url_base,
    repository::{csv_patch_batch_update_exp_dates, get_csv_file_path, BatchExpDateUpdate},
    utils::request,
};
use chrono::Utc;
use log::{debug, warn};
use shared::{error::TuliproxError, model::InputType};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const REFRESH_INTERVAL_SECS: i64 = 24 * 60 * 60;
const EXPIRY_WINDOW_SECS: i64 = 3 * 24 * 60 * 60;
const PANEL_INTERVAL: Duration = Duration::from_mins(5);
const PANEL_INTERVAL_SECS: i64 = 5 * 60;
const FAILURE_COOLDOWN_SECS: i64 = 6 * 60 * 60;
const PERSIST_INTERVAL: Duration = Duration::from_mins(15);

struct Account {
    input_name: Arc<str>,
    name: Arc<str>,
    batch_url: Option<String>,
    url: String,
    source_url: String,
    headers: HashMap<String, String>,
    username: String,
    password: String,
    exp_date: Option<i64>,
    panel: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchError {
    Panel,
    Account,
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ExpiryState {
    last_refresh: HashMap<String, i64>,
    last_attempt: HashMap<String, i64>,
    panel_last_attempt: HashMap<String, i64>,
    cooldown_until: HashMap<String, i64>,
    pending_expiry: HashMap<String, i64>,
}

pub fn exec_xtream_expiry_sync(app_state: &Arc<AppState>, cancel: &CancellationToken) {
    let app_state = Arc::clone(app_state);
    let cancel = cancel.clone();
    tokio::spawn(async move {
        let mut state = load_state(&app_state).await;
        let mut panel_next = HashMap::<String, tokio::time::Instant>::new();
        let mut next_persist = if state.pending_expiry.is_empty() {
            tokio::time::Instant::now() + PERSIST_INTERVAL
        } else {
            tokio::time::Instant::now()
        };
        let mut state_dirty = false;
        loop {
            let now = Utc::now().timestamp();
            let accounts = collect_accounts(&app_state);
            state_dirty |= prune_state(&mut state, &accounts, now);
            fetch_due_accounts(
                &app_state,
                &accounts,
                &mut state,
                &mut panel_next,
                &mut next_persist,
                &mut state_dirty,
                now,
            )
            .await;
            state_dirty |= persist_pending_updates(&app_state, &accounts, &mut state, &mut next_persist, now).await;
            if state_dirty {
                match save_state(&app_state, &state).await {
                    Ok(()) => state_dirty = false,
                    Err(err) => warn!("Failed to persist Xtream expiry state: {err}"),
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(Duration::from_mins(1)) => {}
            }
        }
    });
}

async fn fetch_due_accounts(
    app_state: &Arc<AppState>,
    accounts: &[Account],
    state: &mut ExpiryState,
    panel_next: &mut HashMap<String, tokio::time::Instant>,
    next_persist: &mut tokio::time::Instant,
    state_dirty: &mut bool,
    now: i64,
) {
    let active_panels = accounts.iter().map(panel_key).collect::<HashSet<_>>();
    panel_next.retain(|panel, _| active_panels.contains(panel));
    for account in accounts {
        let key = account_key(account);
        if !is_expiry_refresh_due(
            account.exp_date,
            state.last_refresh.get(&key).copied(),
            state.last_attempt.get(&key).copied(),
            now,
        ) {
            continue;
        }
        let url = account_url(account);
        let panel = panel_key(account);
        let request_at = Utc::now().timestamp();
        if state.cooldown_until.get(&panel).is_some_and(|until| *until > request_at)
            || !is_panel_request_due(state.panel_last_attempt.get(&panel).copied(), request_at)
            || panel_next.get(&panel).is_some_and(|next| *next > tokio::time::Instant::now())
        {
            continue;
        }
        let previous_account_attempt = state.last_attempt.insert(key.clone(), request_at);
        let previous_panel_attempt = state.panel_last_attempt.insert(panel.clone(), request_at);
        let was_dirty = *state_dirty;
        *state_dirty = true;
        if let Err(err) = save_state(app_state, state).await {
            restore_entry(&mut state.last_attempt, key, previous_account_attempt);
            restore_entry(&mut state.panel_last_attempt, panel, previous_panel_attempt);
            *state_dirty = was_dirty;
            warn!("Skipping Xtream expiry request because its throttle state could not be persisted: {err}");
            continue;
        }
        *state_dirty = false;
        panel_next.insert(panel.clone(), tokio::time::Instant::now() + PANEL_INTERVAL);
        let (urgent, result_changed) = match fetch_expiry_date(app_state, account, &url).await {
            Ok(exp_date) => {
                let pending_was_empty = state.pending_expiry.is_empty();
                state.pending_expiry.insert(key, exp_date);
                let expired = is_expired_at(exp_date, Utc::now().timestamp());
                if expired {
                    *next_persist = tokio::time::Instant::now();
                } else if pending_was_empty {
                    *next_persist = tokio::time::Instant::now() + PERSIST_INTERVAL;
                }
                (expired, true)
            }
            Err(FetchError::Panel) => {
                state.cooldown_until.insert(panel, Utc::now().timestamp() + FAILURE_COOLDOWN_SECS);
                (false, true)
            }
            Err(FetchError::Account) => (false, false),
        };
        if result_changed {
            *state_dirty = true;
            match save_state(app_state, state).await {
                Ok(()) => *state_dirty = false,
                Err(err) => {
                    warn!("Failed to checkpoint Xtream expiry result: {err}");
                    *next_persist = tokio::time::Instant::now();
                }
            }
        }
        if urgent {
            break;
        }
    }
}

fn restore_entry(map: &mut HashMap<String, i64>, key: String, previous: Option<i64>) {
    if let Some(previous) = previous {
        map.insert(key, previous);
    } else {
        map.remove(&key);
    }
}

async fn persist_pending_updates(
    app_state: &Arc<AppState>,
    accounts: &[Account],
    state: &mut ExpiryState,
    next_persist: &mut tokio::time::Instant,
    now: i64,
) -> bool {
    if state.pending_expiry.is_empty() || tokio::time::Instant::now() < *next_persist {
        return false;
    }
    let updates = accounts
        .iter()
        .filter_map(|account| {
            let key = account_key(account);
            state.pending_expiry.get(&key).map(|exp_date| (account, *exp_date))
        })
        .collect::<Vec<_>>();
    match persist_updates(app_state, &updates).await {
        Ok(updated_accounts) => {
            for key in updated_accounts {
                state.pending_expiry.remove(&key);
                let refreshed_at = state.last_attempt.get(&key).copied().unwrap_or(now);
                state.last_refresh.insert(key, refreshed_at);
            }
            *next_persist = tokio::time::Instant::now() + PERSIST_INTERVAL;
            true
        }
        Err(err) => {
            warn!("Failed to persist Xtream expiry dates: {err}");
            *next_persist = tokio::time::Instant::now() + Duration::from_mins(1);
            false
        }
    }
}

fn is_expiry_refresh_due(
    exp_date: Option<i64>,
    last_refresh: Option<i64>,
    last_attempt: Option<i64>,
    now: i64,
) -> bool {
    let last_request = last_refresh.max(last_attempt);
    exp_date.is_none_or(|expiry| expiry <= now + EXPIRY_WINDOW_SECS)
        && last_request.is_none_or(|last| now.saturating_sub(last) >= REFRESH_INTERVAL_SECS)
}

fn is_panel_request_due(last_attempt: Option<i64>, now: i64) -> bool {
    last_attempt.is_none_or(|last| now.saturating_sub(last) >= PANEL_INTERVAL_SECS)
}

fn is_expired_at(exp_date: i64, now: i64) -> bool { exp_date <= now }

fn prune_state(state: &mut ExpiryState, accounts: &[Account], now: i64) -> bool {
    let previous_len = state.last_refresh.len()
        + state.last_attempt.len()
        + state.pending_expiry.len()
        + state.panel_last_attempt.len()
        + state.cooldown_until.len();
    let active_keys = accounts.iter().map(account_key).collect::<HashSet<_>>();
    let active_panels = accounts.iter().map(panel_key).collect::<HashSet<_>>();
    state.last_refresh.retain(|key, _| active_keys.contains(key));
    state.last_attempt.retain(|key, _| active_keys.contains(key));
    state.pending_expiry.retain(|key, _| active_keys.contains(key));
    state.panel_last_attempt.retain(|panel, _| active_panels.contains(panel));
    state.cooldown_until.retain(|panel, until| active_panels.contains(panel) && *until > now);
    previous_len
        != state.last_refresh.len()
            + state.last_attempt.len()
            + state.pending_expiry.len()
            + state.panel_last_attempt.len()
            + state.cooldown_until.len()
}

fn collect_accounts(app_state: &AppState) -> Vec<Account> {
    let sources = app_state.app_config.sources.load();
    sources
        .inputs
        .iter()
        .filter(|input| input.enabled && input.input_type.is_xtream())
        .flat_map(|input| {
            let batch_url = input.t_batch_url.clone();
            let panel = Arc::<str>::from(panel_identity(&input.url, &input.name));
            let root = input.username.as_ref().zip(input.password.as_ref()).and_then(|(username, password)| {
                input.resolve_url(&input.url).ok().map(|url| Account {
                    input_name: Arc::clone(&input.name),
                    name: Arc::clone(&input.name),
                    batch_url: batch_url.clone(),
                    url: url.into_owned(),
                    source_url: input.url.clone(),
                    headers: input.headers.clone(),
                    username: username.clone(),
                    password: password.clone(),
                    exp_date: input.exp_date,
                    panel: Arc::clone(&panel),
                })
            });
            root.into_iter().chain(input.aliases.iter().flatten().filter(|alias| alias.enabled).filter_map(
                move |alias| {
                    alias.username.as_ref().zip(alias.password.as_ref()).and_then(|(username, password)| {
                        input.resolve_url(&alias.url).ok().map(|url| Account {
                            input_name: Arc::clone(&input.name),
                            name: Arc::clone(&alias.name),
                            batch_url: batch_url.clone(),
                            url: url.into_owned(),
                            source_url: alias.url.clone(),
                            headers: input.headers.clone(),
                            username: username.clone(),
                            password: password.clone(),
                            exp_date: alias.exp_date,
                            panel: Arc::clone(&panel),
                        })
                    })
                },
            ))
        })
        .collect()
}

fn account_url(account: &Account) -> String {
    get_xtream_stream_url_base(&account.url, &account.username, &account.password)
}

fn panel_identity(source_url: &str, fallback: &str) -> String {
    url::Url::parse(source_url).map_or_else(
        |_| fallback.to_string(),
        |url| {
            if matches!(url.scheme(), "http" | "https") {
                url.origin().ascii_serialization()
            } else {
                url.host_str().map_or_else(|| fallback.to_string(), |host| format!("{}://{host}", url.scheme()))
            }
        },
    )
}

fn panel_key(account: &Account) -> String { account.panel.to_string() }

fn account_key(account: &Account) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        account.input_name.as_ref(),
        account.name.as_ref(),
        account.source_url.as_str(),
        account.username.as_str(),
        account.password.as_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

fn classify_response_status(status: reqwest::StatusCode) -> Result<(), FetchError> {
    if status.is_success() {
        Ok(())
    } else if matches!(status, reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS)
        || status.is_server_error()
    {
        Err(FetchError::Panel)
    } else {
        Err(FetchError::Account)
    }
}

async fn fetch_expiry_date(app_state: &AppState, account: &Account, url: &str) -> Result<i64, FetchError> {
    let client = app_state.http_client.load();
    let response = client
        .get(url)
        .headers(request::get_request_headers(Some(&account.headers), None, None, None))
        .send()
        .await
        .map_err(|err| {
            debug!("Xtream expiry request failed: {err}");
            FetchError::Panel
        })?;
    if let Err(err) = classify_response_status(response.status()) {
        debug!("Xtream expiry request returned {}", response.status());
        return Err(err);
    }
    let value: serde_json::Value = response.json().await.map_err(|err| {
        debug!("Xtream expiry response was invalid JSON: {err}");
        FetchError::Account
    })?;
    parse_expiry_date(&value)
}

fn parse_expiry_date(value: &serde_json::Value) -> Result<i64, FetchError> {
    value
        .pointer("/user_info/exp_date")
        .and_then(|value| value.as_i64().or_else(|| value.as_str().and_then(|value| value.parse().ok())))
        .filter(|timestamp| *timestamp > 0)
        .ok_or(FetchError::Account)
}

fn account_is_current(account: &Account, current_keys: &HashSet<String>) -> bool {
    current_keys.contains(&account_key(account))
}

async fn persist_updates(app_state: &Arc<AppState>, updates: &[(&Account, i64)]) -> Result<Vec<String>, TuliproxError> {
    let current_keys = collect_accounts(app_state).iter().map(account_key).collect::<HashSet<_>>();
    let updates =
        updates.iter().copied().filter(|(account, _)| account_is_current(account, &current_keys)).collect::<Vec<_>>();
    if updates.is_empty() {
        return Ok(Vec::new());
    }
    let now = Utc::now().timestamp();
    let requires_reload =
        updates.iter().any(|(account, exp_date)| account.exp_date != Some(*exp_date) || is_expired_at(*exp_date, now));
    let mut source_updates = Vec::new();
    let mut batch_updates = HashMap::<String, Vec<BatchExpDateUpdate>>::new();
    let mut updated_accounts = Vec::new();
    for (account, exp_date) in updates {
        let disable = is_expired_at(exp_date, now);
        if account.exp_date == Some(exp_date) && !disable {
            updated_accounts.push(account_key(account));
            continue;
        }
        if let Some(batch_url) = &account.batch_url {
            batch_updates.entry(batch_url.clone()).or_default().push(BatchExpDateUpdate {
                account_key: account_key(account),
                account_name: Arc::clone(&account.name),
                exp_date,
                disable,
            });
        } else {
            source_updates.push((
                account_key(account),
                Arc::clone(&account.input_name),
                Arc::clone(&account.name),
                exp_date,
            ));
        }
    }
    let sources_path = app_state.app_config.paths.load().sources_file_path.clone();
    let sources_path = std::path::Path::new(&sources_path);
    if !source_updates.is_empty() {
        let _sources_lock = app_state.app_config.file_locks.write_lock(sources_path).await;
        let mut sources = read_sources_file_from_path(sources_path, false, false, None).await?;
        let mut source_changed = false;
        for (key, input_name, account_name, exp_date) in source_updates {
            if let Some(input) = sources.inputs.iter_mut().find(|input| input.name == input_name) {
                source_changed |=
                    input.update_account_expiration_date(&account_name, exp_date, is_expired_at(exp_date, now))?;
                updated_accounts.push(key);
            }
        }
        if source_changed {
            persist_source_config_preserving_templates(&app_state.app_config, Some(sources_path), sources).await?;
            app_state
                .app_config
                .file_locks
                .mark_internal_write_revision(sources_path)
                .await
                .map_err(|err| TuliproxError::Io(format!("Failed to track internal source update: {err}")))?;
        }
    }
    let mut persistence_error = None;
    for (batch_url, updates) in batch_updates {
        let csv_path = match get_csv_file_path(&batch_url) {
            Ok(path) => path,
            Err(err) => {
                persistence_error = Some(TuliproxError::ConfigInput(format!("{err}")));
                break;
            }
        };
        let _csv_lock = app_state.app_config.file_locks.write_lock(&csv_path).await;
        let backup_dir = app_state.app_config.config.load().get_backup_dir().to_string();
        match csv_patch_batch_update_exp_dates(InputType::XtreamBatch, &csv_path, &updates, &backup_dir).await {
            Ok((batch_changed, matched_keys)) => {
                if batch_changed {
                    app_state.app_config.file_locks.mark_internal_write_revision(&csv_path).await.map_err(|err| {
                        TuliproxError::Io(format!("Failed to track internal alias CSV update: {err}"))
                    })?;
                }
                updated_accounts.extend(matched_keys);
            }
            Err(err) => {
                persistence_error = Some(err);
                break;
            }
        }
    }
    if requires_reload && !updated_accounts.is_empty() {
        ConfigFile::load_sources(app_state).await?;
    }
    if let Some(err) = persistence_error {
        return Err(err);
    }
    Ok(updated_accounts)
}

fn state_path(app_state: &AppState) -> std::path::PathBuf {
    std::path::PathBuf::from(&app_state.app_config.config.load().storage_dir).join("xtream_expiry_state.json")
}

async fn load_state(app_state: &AppState) -> ExpiryState {
    let path = state_path(app_state);
    match load_state_from_path(&path).await {
        Ok(state) => state,
        Err(err) => {
            warn!("Ignoring unreadable Xtream expiry state: {err}");
            ExpiryState::default()
        }
    }
}

async fn save_state(app_state: &AppState, state: &ExpiryState) -> Result<(), TuliproxError> {
    let path = state_path(app_state);
    save_state_to_path(&path, state).await
}

async fn load_state_from_path(path: &std::path::Path) -> Result<ExpiryState, TuliproxError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| TuliproxError::ConfigInput(format!("Invalid Xtream expiry state: {err}"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ExpiryState::default()),
        Err(err) => Err(TuliproxError::Io(format!("Failed to read Xtream expiry state: {err}"))),
    }
}

async fn save_state_to_path(path: &std::path::Path, state: &ExpiryState) -> Result<(), TuliproxError> {
    let parent = path.parent().ok_or_else(|| {
        TuliproxError::ConfigInput(format!("Xtream expiry state path has no parent: {}", path.display()))
    })?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|err| TuliproxError::Io(format!("Failed to create Xtream expiry state directory: {err}")))?;
    let content = serde_json::to_vec(state)
        .map_err(|err| TuliproxError::ConfigInput(format!("Failed to serialize Xtream expiry state: {err}")))?;
    let tmp_path = path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, content)
        .await
        .map_err(|err| TuliproxError::Io(format!("Failed to write Xtream expiry state: {err}")))?;
    if let Err(err) = tokio::fs::rename(&tmp_path, path).await {
        if let Err(cleanup_err) = tokio::fs::remove_file(&tmp_path).await {
            warn!("Failed to remove temporary Xtream expiry state after rename error: {cleanup_err}");
        }
        return Err(TuliproxError::Io(format!("Failed to replace Xtream expiry state: {err}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        account_key, classify_response_status, load_state_from_path, parse_expiry_date, save_state_to_path, Account,
        ExpiryState, FetchError,
    };
    use reqwest::StatusCode;
    use shared::model::{ConfigInputAliasDto, ConfigInputDto};
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    };

    fn account(password: &str) -> Account {
        Account {
            input_name: Arc::from("input"),
            name: Arc::from("alias"),
            batch_url: None,
            url: "http://panel.example".to_string(),
            source_url: "http://panel.example".to_string(),
            headers: HashMap::new(),
            username: "user".to_string(),
            password: password.to_string(),
            exp_date: None,
            panel: Arc::from("http://panel.example"),
        }
    }

    #[test]
    fn missing_or_soon_expiring_accounts_are_due_once_per_day() {
        let now = 1_000_000;
        assert!(super::is_expiry_refresh_due(None, None, None, now));
        assert!(!super::is_expiry_refresh_due(Some(now + 2 * 24 * 60 * 60), Some(now - 1), None, now));
        assert!(super::is_expiry_refresh_due(Some(now + 2 * 24 * 60 * 60), Some(now - 24 * 60 * 60), None, now));
        assert!(!super::is_expiry_refresh_due(Some(now + 4 * 24 * 60 * 60), None, None, now));
        assert!(!super::is_expiry_refresh_due(None, None, Some(now - 1), now));
    }

    #[test]
    fn persisted_panel_attempt_enforces_spacing_after_restart() {
        let now = 1_000_000;
        assert!(!super::is_panel_request_due(Some(now - 299), now));
        assert!(super::is_panel_request_due(Some(now - 300), now));
    }

    #[test]
    fn expiry_at_or_before_now_is_expired() {
        assert!(super::is_expired_at(99, 100));
        assert!(super::is_expired_at(100, 100));
        assert!(!super::is_expired_at(101, 100));
    }

    #[test]
    fn state_pruning_keeps_only_active_accounts_and_panels() {
        let active = account("password");
        let active_key = account_key(&active);
        let active_panel = super::panel_key(&active);
        let mut state = ExpiryState::default();
        for values in [&mut state.last_refresh, &mut state.last_attempt, &mut state.pending_expiry] {
            values.insert(active_key.clone(), 10);
            values.insert("stale".to_string(), 20);
        }
        state.panel_last_attempt.insert(active_panel.clone(), 10);
        state.panel_last_attempt.insert("http://stale.example".to_string(), 20);
        state.cooldown_until.insert(active_panel.clone(), 200);
        state.cooldown_until.insert("http://stale.example".to_string(), 200);

        assert!(super::prune_state(&mut state, std::slice::from_ref(&active), 100));

        assert_eq!(state.last_attempt.len(), 1);
        assert_eq!(state.last_refresh.len(), 1);
        assert_eq!(state.pending_expiry.len(), 1);
        assert_eq!(state.panel_last_attempt.len(), 1);
        assert_eq!(state.cooldown_until.len(), 1);
        assert!(!super::prune_state(&mut state, std::slice::from_ref(&active), 100));
    }

    #[test]
    fn account_key_changes_with_credentials() {
        assert_ne!(account_key(&account("old")), account_key(&account("new")));
    }

    #[test]
    fn stale_account_snapshot_is_rejected() {
        let stale = account("old");
        let current = account("new");

        let current_keys = HashSet::from([account_key(&current)]);
        assert!(!super::account_is_current(&stale, &current_keys));
        assert!(super::account_is_current(&current, &current_keys));
    }

    #[test]
    fn inputs_with_same_configured_panel_share_throttle_identity() {
        assert_eq!(
            super::panel_identity("http://panel.example/account-a", "first"),
            super::panel_identity("http://panel.example/account-b", "second")
        );
    }

    #[test]
    fn account_key_ignores_resolved_provider_rotation() {
        let first = account("password");
        let mut second = account("password");
        second.url = "http://192.0.2.10".to_string();

        assert_eq!(account_key(&first), account_key(&second));
    }

    #[test]
    fn aliases_share_their_input_panel_throttle() {
        let first = account("first-password");
        let mut alias = account("alias-password");
        alias.name = Arc::from("alias");
        alias.url = "http://alternate.example".to_string();

        assert_eq!(super::panel_key(&first), super::panel_key(&alias));
    }

    #[test]
    fn only_panel_failures_trigger_panel_cooldown() {
        assert_eq!(classify_response_status(StatusCode::TOO_MANY_REQUESTS), Err(FetchError::Panel));
        assert_eq!(classify_response_status(StatusCode::FORBIDDEN), Err(FetchError::Panel));
        assert_eq!(classify_response_status(StatusCode::SERVICE_UNAVAILABLE), Err(FetchError::Panel));
        assert_eq!(classify_response_status(StatusCode::UNAUTHORIZED), Err(FetchError::Account));
        assert_eq!(classify_response_status(StatusCode::NOT_FOUND), Err(FetchError::Account));
        assert_eq!(classify_response_status(StatusCode::OK), Ok(()));
    }

    #[test]
    fn expiry_response_accepts_xtream_string_and_number_formats() {
        assert_eq!(parse_expiry_date(&serde_json::json!({"user_info": {"exp_date": "2000000000"}})), Ok(2_000_000_000));
        assert_eq!(
            parse_expiry_date(&serde_json::json!({"user_info": {"exp_date": 2_000_000_000}})),
            Ok(2_000_000_000)
        );
        assert_eq!(parse_expiry_date(&serde_json::json!({"user_info": {"exp_date": null}})), Err(FetchError::Account));
        assert_eq!(parse_expiry_date(&serde_json::json!({"user_info": {"exp_date": "0"}})), Err(FetchError::Account));
        assert_eq!(parse_expiry_date(&serde_json::json!({"user_info": {"exp_date": -1}})), Err(FetchError::Account));
    }

    #[test]
    fn source_alias_update_preserves_unresolved_credentials() -> Result<(), Box<dyn std::error::Error>> {
        let mut input = ConfigInputDto {
            name: Arc::from("input"),
            url: "http://root.example".to_string(),
            username: Some("shared".to_string()),
            password: Some("root-password".to_string()),
            exp_date: Some(10),
            aliases: Some(vec![ConfigInputAliasDto {
                name: Arc::from("alias"),
                url: "http://alias.example".to_string(),
                username: Some("${env:XTREAM_USER}".to_string()),
                password: Some("${env:XTREAM_PASSWORD}".to_string()),
                exp_date: Some(20),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert!(input.update_account_expiration_date("alias", 30, false)?);
        assert_eq!(input.exp_date, Some(10));
        let alias = input.aliases.as_ref().and_then(|aliases| aliases.first()).ok_or("missing alias")?;
        assert_eq!(alias.exp_date, Some(30));
        assert_eq!(alias.username.as_deref(), Some("${env:XTREAM_USER}"));
        assert_eq!(alias.password.as_deref(), Some("${env:XTREAM_PASSWORD}"));
        Ok(())
    }

    #[test]
    fn expired_source_account_is_disabled() -> Result<(), Box<dyn std::error::Error>> {
        let mut input = ConfigInputDto { name: Arc::from("input"), enabled: true, ..Default::default() };

        assert!(input.update_account_expiration_date("input", 30, true)?);
        assert_eq!(input.exp_date, Some(30));
        assert!(!input.enabled);
        Ok(())
    }

    #[tokio::test]
    async fn expiry_state_is_atomically_replaced() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("xtream_expiry_state.json");
        tokio::fs::write(&path, b"{}").await?;
        let mut state = ExpiryState::default();
        state.last_refresh.insert("input/account".to_string(), 42);

        save_state_to_path(&path, &state).await?;

        assert_eq!(load_state_from_path(&path).await?, state);
        assert!(!path.with_extension("json.tmp").exists());
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_expiry_state_is_reported() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("xtream_expiry_state.json");
        tokio::fs::write(&path, b"not-json").await?;

        assert!(load_state_from_path(&path).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_expiry_state_loads_with_new_fields_defaulted() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("xtream_expiry_state.json");
        tokio::fs::write(&path, br#"{"last_refresh":{"account":42},"cooldown_until":{}}"#).await?;

        let state = load_state_from_path(&path).await?;

        assert_eq!(state.last_refresh.get("account"), Some(&42));
        assert!(state.last_attempt.is_empty());
        assert!(state.panel_last_attempt.is_empty());
        assert!(state.pending_expiry.is_empty());
        Ok(())
    }
}
