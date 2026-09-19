// Environment file (.env) discovery and loading.
//
// Allows storing secrets and credentials in `.env` files, which are loaded
// at process startup and resolved via `${env:VAR}` in configuration files.

use shared::defaults::CONFIG_PATH;
use std::path::{Path, PathBuf};
use tuliprox_core::utils;

// Discovers candidate paths for a `.env` file in order of precedence:
// 1. Explicit config file parent directory (`<config_file_dir>/.env`) if `-c` was supplied
// 2. Config directory (`<config_dir>/.env`)
// 3. Home directory (`<home_dir>/.env`)
// 4. Current working directory (`./.env`)
pub fn find_candidate_env_paths(
    config_file: Option<&str>,
    config_path: Option<&str>,
    home: Option<&str>,
) -> Vec<PathBuf> {
    let home_path = home
        .filter(|p| !p.trim().is_empty())
        .map_or_else(utils::get_home_path, |p| PathBuf::from(utils::resolve_env_var(p)));

    let config_dir = config_path.filter(|p| !p.trim().is_empty()).map_or_else(
        || utils::get_default_path_for_home(Path::new(&home_path), CONFIG_PATH),
        |p| PathBuf::from(utils::resolve_env_var(p)),
    );

    let mut candidates = Vec::new();

    if let Some(cfg) = config_file.filter(|p| !p.trim().is_empty()) {
        if let Some(parent) = Path::new(cfg).parent() {
            let p = parent.join(".env");
            if !candidates.contains(&p) {
                candidates.push(p);
            }
        }
    }

    let config_env = config_dir.join(".env");
    if !candidates.contains(&config_env) {
        candidates.push(config_env);
    }

    let home_env = home_path.join(".env");
    if !candidates.contains(&home_env) {
        candidates.push(home_env);
    }

    if let Ok(cwd) = std::env::current_dir() {
        let cwd_env = cwd.join(".env");
        if !candidates.contains(&cwd_env) {
            candidates.push(cwd_env);
        }
    }

    candidates
}

// Loads environment variables from an explicit or discovered `.env` file into the process environment.
//
// Follows standard 12-factor principles: variables already present in the environment
pub const DEFAULT_ENV_FILE_ENV_VAR: &str = "TULIPROX_ENV_FILE";

// Loads environment variables from an explicit or discovered `.env` file into the process environment.
//
// Follows standard 12-factor principles: variables already present in the environment
// (e.g. from the OS, Docker, or systemd) are never overwritten by `.env`.
//
// Priority for explicit path:
// 1. CLI argument `--env-file` (`-e`)
// 2. Environment variable `TULIPROX_ENV_FILE`
//
// Returns `Ok(Some(path))` with the path of the file that was loaded, or `Ok(None)` if no `.env` file was found.
// If an explicit `--env-file` or `TULIPROX_ENV_FILE` was provided but cannot be found or read, or if an existing
// `.env` file has invalid syntax, returns `Err(String)`.
pub fn load_env_file(
    explicit_env_file: Option<&str>,
    config_file: Option<&str>,
    config_path: Option<&str>,
    home: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    let explicit_file = explicit_env_file
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var(DEFAULT_ENV_FILE_ENV_VAR).ok().filter(|p| !p.trim().is_empty()).map(PathBuf::from));

    if let Some(path) = explicit_file {
        if !path.is_file() {
            return Err(format!("Specified environment file does not exist or is not a file: {}", path.display()));
        }
        dotenvy::from_path(&path)
            .map_err(|err| format!("Failed to load environment file '{}': {err}", path.display()))?;
        return Ok(Some(path));
    }

    let candidates = find_candidate_env_paths(config_file, config_path, home);
    for candidate in candidates {
        if candidate.is_file() {
            dotenvy::from_path(&candidate)
                .map_err(|err| format!("Failed to load environment file '{}': {err}", candidate.display()))?;
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_candidate_discovery_order() {
        let candidates = find_candidate_env_paths(
            Some("/custom/cfg/config.yml"),
            Some("/custom/config_dir"),
            Some("/custom/home_dir"),
        );
        assert_eq!(candidates[0], PathBuf::from("/custom/cfg/.env"));
        assert_eq!(candidates[1], PathBuf::from("/custom/config_dir/.env"));
        assert_eq!(candidates[2], PathBuf::from("/custom/home_dir/.env"));
    }

    #[test]
    fn test_candidate_discovery_with_env_var_resolution() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let var_home = format!("TULIPROX_TEST_HOME_{}", fastrand::u64(..));
        let var_cfg = format!("TULIPROX_TEST_CFG_{}", fastrand::u64(..));

        std::env::set_var(&var_home, "/resolved_home");
        std::env::set_var(&var_cfg, "/resolved_cfg");

        let candidates = find_candidate_env_paths(
            None,
            Some(&format!("${{env:{var_cfg}}}/custom")),
            Some(&format!("${{env:{var_home}}}/myhome")),
        );

        assert_eq!(candidates[0], PathBuf::from("/resolved_cfg/custom/.env"));
        assert_eq!(candidates[1], PathBuf::from("/resolved_home/myhome/.env"));

        std::env::remove_var(&var_home);
        std::env::remove_var(&var_cfg);
    }

    #[test]
    fn test_candidate_deduplication() {
        let candidates =
            find_candidate_env_paths(Some("/same/dir/config.yml"), Some("/same/dir"), Some("/custom/home"));
        let count = candidates.iter().filter(|p| *p == &PathBuf::from("/same/dir/.env")).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_explicit_env_file_not_found() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = load_env_file(Some("/nonexistent/file/.env"), None, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist or is not a file"));
    }

    #[test]
    fn test_explicit_env_file_success_and_no_overwrite() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        let unique_var = format!("TULIPROX_TEST_VAR_{}", fastrand::u64(..));
        let unique_preexisting = format!("TULIPROX_PREEXISTING_{}", fastrand::u64(..));

        std::env::set_var(&unique_preexisting, "initial_value");

        let content = format!("{unique_var}=hello_world\n{unique_preexisting}=overwritten_value\n");
        fs::write(&env_file, content).expect("write .env");

        let loaded = load_env_file(Some(env_file.to_str().unwrap()), None, None, None);
        assert!(loaded.is_ok());
        assert_eq!(loaded.unwrap(), Some(env_file));

        assert_eq!(std::env::var(&unique_var).as_deref(), Ok("hello_world"));
        // Pre-existing env vars must NOT be overwritten
        assert_eq!(std::env::var(&unique_preexisting).as_deref(), Ok("initial_value"));

        std::env::remove_var(&unique_var);
        std::env::remove_var(&unique_preexisting);
    }

    #[test]
    fn test_discovered_env_file_loaded() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        let unique_var = format!("TULIPROX_DISCOVERED_{}", fastrand::u64(..));

        fs::write(&env_file, format!("{unique_var}=discovered_val\n")).expect("write .env");

        let loaded = load_env_file(None, None, Some(dir.path().to_str().unwrap()), None);
        assert!(loaded.is_ok());
        assert_eq!(loaded.unwrap(), Some(env_file));
        assert_eq!(std::env::var(&unique_var).as_deref(), Ok("discovered_val"));

        std::env::remove_var(&unique_var);
    }

    #[test]
    fn test_env_var_override_path() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempdir().expect("tempdir");
        let env_file = dir.path().join("my_custom.env");
        let unique_var = format!("TULIPROX_CUSTOM_ENV_{}", fastrand::u64(..));

        fs::write(&env_file, format!("{unique_var}=custom_via_env_var\n")).expect("write .env");

        std::env::set_var(DEFAULT_ENV_FILE_ENV_VAR, env_file.to_str().unwrap());

        let loaded = load_env_file(None, None, None, None);
        assert!(loaded.is_ok());
        assert_eq!(loaded.unwrap(), Some(env_file));
        assert_eq!(std::env::var(&unique_var).as_deref(), Ok("custom_via_env_var"));

        std::env::remove_var(DEFAULT_ENV_FILE_ENV_VAR);
        std::env::remove_var(&unique_var);
    }

    #[test]
    fn test_no_env_file_returns_ok_none() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempdir().expect("tempdir");
        let non_existing_config = dir.path().join("does_not_exist_cfg");
        let non_existing_home = dir.path().join("does_not_exist_home");

        let loaded = load_env_file(
            None,
            None,
            Some(non_existing_config.to_str().unwrap()),
            Some(non_existing_home.to_str().unwrap()),
        );

        assert!(loaded.is_ok());
        assert_eq!(loaded.unwrap(), None);
    }
}
