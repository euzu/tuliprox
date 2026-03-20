use std::fmt;
use std::fs::File;
use std::io::{BufRead, ErrorKind};
use std::path::{Path, PathBuf};

use log::{debug, error, warn};
use shared::error::{info_err_res, TuliproxError};
use shared::model::permission::{permission_from_name, PermissionSet, PERM_ALL};
use shared::model::WebAuthConfigDto;

use crate::model::macros;
use crate::utils;

#[derive(Clone)]
pub struct WebUiUser {
    pub username: String,
    pub password_hash: String,
    pub groups: Vec<String>,
}

impl fmt::Debug for WebUiUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebUiUser")
            .field("username", &self.username)
            .field("password_hash", &"*****")
            .field("groups", &self.groups)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RbacGroup {
    pub name: String,
    pub permissions: PermissionSet,
}

#[derive(Debug, Clone)]
pub struct WebAuthConfig {
    pub enabled: bool,
    pub issuer: String,
    pub secret: String,
    pub token_ttl_mins: u32,
    pub userfile: Option<String>,
    pub groupfile: Option<String>,
    pub t_users: Option<Vec<WebUiUser>>,
    pub t_groups: Option<Vec<RbacGroup>>,
}

macros::from_impl!(WebAuthConfig);

impl From<&WebAuthConfigDto> for WebAuthConfig {
    fn from(dto: &WebAuthConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            issuer: dto.issuer.clone(),
            secret: dto.secret.clone(),
            token_ttl_mins: dto.token_ttl_mins,
            userfile: dto.userfile.clone(),
            groupfile: dto.groupfile.clone(),
            t_users: None,
            t_groups: None,
        }
    }
}

impl From<&WebAuthConfig> for WebAuthConfigDto {
    fn from(instance: &WebAuthConfig) -> Self {
        Self {
            enabled: instance.enabled,
            issuer: instance.issuer.clone(),
            secret: instance.secret.clone(),
            token_ttl_mins: instance.token_ttl_mins,
            userfile: instance.userfile.clone(),
            groupfile: instance.groupfile.clone(),
        }
    }
}

impl WebAuthConfig {
    pub fn prepare(&mut self, config_path: &str) -> Result<(), TuliproxError> {
        let has_custom_userfile = !utils::is_blank_or_default_user_file_path(&self.userfile);
        let has_custom_groupfile = !utils::is_blank_or_default_user_group_file_path(&self.groupfile);
        let userfile_name = if has_custom_userfile {
            self.userfile.as_ref().map_or_else(String::new, std::borrow::ToOwned::to_owned)
        } else {
            utils::get_default_user_file_path(config_path)
        };
        let groupfile_name = if has_custom_groupfile {
            self.groupfile.as_ref().map_or_else(String::new, std::borrow::ToOwned::to_owned)
        } else {
            utils::get_default_user_group_file_path(config_path)
        };
        self.userfile = Some(userfile_name.clone());
        self.groupfile = Some(groupfile_name.clone());

        let userfile_path = if has_custom_userfile {
            Self::resolve_auth_file_path(config_path, &userfile_name)
        } else {
            PathBuf::from(&userfile_name)
        };
        if !utils::path_exists(&userfile_path) {
            return info_err_res!("Could not find userfile {}", &userfile_name);
        }

        let Ok(file) = File::open(&userfile_path) else {
            return info_err_res!("Could not read userfile {:?}", &userfile_path);
        };
        let reader = utils::file_reader(file);
        let mut users = vec![];
        for credentials in reader.lines() {
            match credentials {
                Ok(credentials) => {
                    if let Some(user) = Self::parse_user_line(&credentials) {
                        users.push(user);
                    }
                }
                Err(err) => {
                    warn!("Could not read line from userfile {}: {err}", userfile_path.display());
                }
            }
        }
        self.t_users = Some(users);

        let groupfile_path = if has_custom_groupfile {
            Self::resolve_auth_file_path(config_path, &groupfile_name)
        } else {
            PathBuf::from(&groupfile_name)
        };
        debug!(
            "Web auth prepare: config_path='{}', userfile='{}', resolved_userfile='{}', groupfile='{}', resolved_groupfile='{}'",
            config_path,
            userfile_name,
            userfile_path.display(),
            groupfile_name,
            groupfile_path.display()
        );
        self.t_groups = Some(Self::parse_groups(&groupfile_path));
        self.validate_user_groups();

        Ok(())
    }

    pub fn get_user_password(&self, username: &str) -> Option<&str> {
        self.t_users.as_ref().and_then(|users| {
            users.iter().find(|credential| credential.username.eq_ignore_ascii_case(username)).map(|credential| credential.password_hash.as_str())
        })
    }

    pub fn resolve_permissions(&self, username: &str) -> PermissionSet {
        let Some(users) = &self.t_users else {
            debug!("Web auth resolve_permissions('{username}'): no users loaded");
            return PermissionSet::new();
        };

        let Some(user) = users.iter().find(|candidate| candidate.username.eq_ignore_ascii_case(username)) else {
            debug!("Web auth resolve_permissions('{username}'): user not found");
            return PermissionSet::new();
        };

        if user.groups.iter().any(|group| group.eq_ignore_ascii_case("admin")) {
            debug!("Web auth resolve_permissions('{username}'): admin user -> all permissions");
            return PERM_ALL;
        }

        let mut permissions = PermissionSet::new();
        if let Some(groups) = &self.t_groups {
            for group_name in &user.groups {
                if let Some(group) = groups.iter().find(|candidate| candidate.name.eq_ignore_ascii_case(group_name)) {
                    permissions.union(group.permissions);
                } else {
                    debug!("Web auth resolve_permissions('{username}'): group '{group_name}' not found");
                }
            }
        } else {
            debug!("Web auth resolve_permissions('{username}'): no groups loaded");
        }
        debug!(
            "Web auth resolve_permissions('{username}'): user_groups={:?}, resolved_permissions={permissions}",
            user.groups
        );
        permissions
    }

    pub fn pwd_version_from_hash(hash: &str) -> u32 {
        let digest = blake3::hash(hash.as_bytes());
        let bytes = digest.as_bytes();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn parse_user_line(line: &str) -> Option<WebUiUser> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }

        let mut parts = trimmed.splitn(3, ':');
        let (Some(username), Some(password_hash)) = (parts.next(), parts.next()) else {
            return None;
        };

        let mut groups = parts
            .next()
            .map(|groups_str| {
                groups_str
                    .split(',')
                    .map(str::trim)
                    .filter(|group| !group.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if groups.is_empty() {
            groups.push(String::from("admin"));
        }

        Some(WebUiUser {
            username: username.trim().to_string(),
            password_hash: password_hash.trim().to_string(),
            groups,
        })
    }

    fn resolve_auth_file_path(config_path: &str, file_name: &str) -> PathBuf {
        let file_path = PathBuf::from(file_name);
        if file_path.is_absolute() || utils::path_exists(&file_path) {
            file_path
        } else {
            PathBuf::from(config_path).join(file_name)
        }
    }

    fn parse_groups(groupfile_path: &Path) -> Vec<RbacGroup> {
        match File::open(groupfile_path) {
            Ok(file) => {
                let reader = utils::file_reader(file);
                let mut groups: Vec<RbacGroup> = vec![];
                for line in reader.lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(err) => {
                            warn!("Could not read line from groups file {}: {err}", groupfile_path.display());
                            continue;
                        }
                    };

                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }

                    let mut parts = trimmed.splitn(2, ':');
                    let (Some(name), Some(perms_str)) = (parts.next(), parts.next()) else {
                        continue;
                    };

                    let name = name.trim();
                    if name.eq_ignore_ascii_case("admin") {
                        warn!("groups file {}: 'admin' is reserved and cannot be redefined, skipping", groupfile_path.display());
                        continue;
                    }

                    let mut permissions = PermissionSet::new();
                    for perm_name in perms_str.split(',').map(str::trim).filter(|perm| !perm.is_empty()) {
                        match permission_from_name(perm_name) {
                            Some(permission) => permissions.set(permission),
                            None => warn!("groups file {}: unknown permission '{perm_name}' in group '{name}', ignoring", groupfile_path.display()),
                        }
                    }

                    if let Some(existing_group) = groups.iter_mut().find(|group| group.name.eq_ignore_ascii_case(name)) {
                        warn!("groups file {}: duplicate group '{name}' redefined, replacing prior definition", groupfile_path.display());
                        existing_group.permissions = permissions;
                    } else {
                        groups.push(RbacGroup { name: name.to_string(), permissions });
                    }
                }
                groups
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                debug!("groups file {} not found, using only built-in admin permissions", groupfile_path.display());
                vec![]
            }
            Err(err) => {
                error!("Could not read groups file {}: {err}, using only built-in admin permissions", groupfile_path.display());
                vec![]
            }
        }
    }

    pub fn validate_user_groups(&self) {
        let Some(users) = &self.t_users else {
            return;
        };

        for user in users {
            for group_name in &user.groups {
                if group_name.eq_ignore_ascii_case("admin") {
                    continue;
                }

                let group_exists = self.t_groups.as_ref().is_some_and(|groups| {
                    groups.iter().any(|group| group.name.eq_ignore_ascii_case(group_name))
                });

                if !group_exists {
                    warn!("user '{}' references unknown group '{}', it will have no effect", user.username, group_name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::permission::{Permission, PermissionSet, PERM_ALL};
    use std::{io::Write, path::Path};
    use tempfile::NamedTempFile;

    struct DirGuard {
        previous_dir: PathBuf,
    }

    impl DirGuard {
        fn enter(path: &Path) -> std::io::Result<Self> {
            let previous_dir = std::env::current_dir()?;
            std::env::set_current_dir(path)?;
            Ok(Self { previous_dir })
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous_dir);
        }
    }

    // --- parse_user_line tests ---

    #[test]
    fn test_parse_user_line_no_groups_defaults_to_admin() {
        let user = WebAuthConfig::parse_user_line("admin:$argon2id$hash123");
        assert!(user.is_some());
        let user = user.expect("should parse");
        assert_eq!(user.username, "admin");
        assert_eq!(user.password_hash, "$argon2id$hash123");
        assert_eq!(user.groups, vec!["admin"]);
    }

    #[test]
    fn test_parse_user_line_with_groups() {
        let user = WebAuthConfig::parse_user_line("alice:hash123:viewer,source_manager");
        assert!(user.is_some());
        let user = user.expect("should parse");
        assert_eq!(user.username, "alice");
        assert_eq!(user.password_hash, "hash123");
        assert_eq!(user.groups, vec!["viewer", "source_manager"]);
    }

    #[test]
    fn test_parse_user_line_empty_line() {
        assert!(WebAuthConfig::parse_user_line("").is_none());
    }

    #[test]
    fn test_parse_user_line_comment() {
        assert!(WebAuthConfig::parse_user_line("# this is a comment").is_none());
    }

    #[test]
    fn test_parse_user_line_whitespace_only() {
        assert!(WebAuthConfig::parse_user_line("   ").is_none());
    }

    #[test]
    fn test_parse_user_line_no_password() {
        // Only username, no colon separator for password
        assert!(WebAuthConfig::parse_user_line("justusername").is_none());
    }

    #[test]
    fn test_parse_user_line_trims_whitespace() {
        let user = WebAuthConfig::parse_user_line("  bob : hash456 : groupA , groupB ");
        assert!(user.is_some());
        let user = user.expect("should parse");
        assert_eq!(user.username, "bob");
        assert_eq!(user.password_hash, "hash456");
        assert_eq!(user.groups, vec!["groupA", "groupB"]);
    }

    #[test]
    fn test_parse_user_line_empty_groups_defaults_to_admin() {
        let user = WebAuthConfig::parse_user_line("user:hash:");
        assert!(user.is_some());
        let user = user.expect("should parse");
        assert_eq!(user.groups, vec!["admin"]);
    }

    // --- parse_groups tests ---

    #[test]
    fn test_parse_groups_valid() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "viewer:config.read,source.read").expect("write");
        writeln!(file, "editor:config.read,config.write").expect("write");
        let groups = WebAuthConfig::parse_groups(file.path());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "viewer");
        assert!(groups[0].permissions.contains(Permission::ConfigRead));
        assert!(groups[0].permissions.contains(Permission::SourceRead));
        assert!(!groups[0].permissions.contains(Permission::ConfigWrite));
        assert_eq!(groups[1].name, "editor");
        assert!(groups[1].permissions.contains(Permission::ConfigRead));
        assert!(groups[1].permissions.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_parse_groups_skips_comments_and_empty() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "# comment").expect("write");
        writeln!(file).expect("write");
        writeln!(file, "viewer:config.read").expect("write");
        let groups = WebAuthConfig::parse_groups(file.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "viewer");
    }

    #[test]
    fn test_parse_groups_admin_reserved() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "admin:config.read").expect("write");
        writeln!(file, "viewer:source.read").expect("write");
        let groups = WebAuthConfig::parse_groups(file.path());
        // admin line should be skipped
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "viewer");
    }

    #[test]
    fn test_parse_groups_duplicate_replaces() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "viewer:config.read").expect("write");
        writeln!(file, "viewer:source.read").expect("write");
        let groups = WebAuthConfig::parse_groups(file.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "viewer");
        // Second definition replaces first
        assert!(!groups[0].permissions.contains(Permission::ConfigRead));
        assert!(groups[0].permissions.contains(Permission::SourceRead));
    }

    #[test]
    fn test_parse_groups_unknown_permission_ignored() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "viewer:config.read,fake.perm,source.read").expect("write");
        let groups = WebAuthConfig::parse_groups(file.path());
        assert_eq!(groups.len(), 1);
        assert!(groups[0].permissions.contains(Permission::ConfigRead));
        assert!(groups[0].permissions.contains(Permission::SourceRead));
    }

    #[test]
    fn test_parse_groups_missing_file() {
        let groups = WebAuthConfig::parse_groups(std::path::Path::new("/nonexistent/groups.txt"));
        assert!(groups.is_empty());
    }

    // --- resolve_permissions tests ---

    #[test]
    fn test_resolve_permissions_admin_user() {
        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![WebUiUser {
                username: "admin".to_string(),
                password_hash: "hash".to_string(),
                groups: vec!["admin".to_string()],
            }]),
            t_groups: Some(vec![]),
        };
        let perms = config.resolve_permissions("admin");
        assert_eq!(perms, PERM_ALL);
    }

    #[test]
    fn test_resolve_permissions_grouped_user() {
        let mut viewer_perms = PermissionSet::new();
        viewer_perms.set(Permission::ConfigRead);
        viewer_perms.set(Permission::SourceRead);

        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![WebUiUser {
                username: "alice".to_string(),
                password_hash: "hash".to_string(),
                groups: vec!["viewer".to_string()],
            }]),
            t_groups: Some(vec![RbacGroup {
                name: "viewer".to_string(),
                permissions: viewer_perms,
            }]),
        };
        let perms = config.resolve_permissions("alice");
        assert!(perms.contains(Permission::ConfigRead));
        assert!(perms.contains(Permission::SourceRead));
        assert!(!perms.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_resolve_permissions_multiple_groups_union() {
        let mut viewer_perms = PermissionSet::new();
        viewer_perms.set(Permission::ConfigRead);

        let mut editor_perms = PermissionSet::new();
        editor_perms.set(Permission::SourceWrite);

        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![WebUiUser {
                username: "bob".to_string(),
                password_hash: "hash".to_string(),
                groups: vec!["viewer".to_string(), "editor".to_string()],
            }]),
            t_groups: Some(vec![
                RbacGroup {
                    name: "viewer".to_string(),
                    permissions: viewer_perms,
                },
                RbacGroup {
                    name: "editor".to_string(),
                    permissions: editor_perms,
                },
            ]),
        };
        let perms = config.resolve_permissions("bob");
        assert!(perms.contains(Permission::ConfigRead));
        assert!(perms.contains(Permission::SourceWrite));
    }

    #[test]
    fn test_resolve_permissions_admin_overrides_groups() {
        let mut viewer_perms = PermissionSet::new();
        viewer_perms.set(Permission::ConfigRead);

        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![WebUiUser {
                username: "superuser".to_string(),
                password_hash: "hash".to_string(),
                groups: vec!["admin".to_string(), "viewer".to_string()],
            }]),
            t_groups: Some(vec![RbacGroup {
                name: "viewer".to_string(),
                permissions: viewer_perms,
            }]),
        };
        let perms = config.resolve_permissions("superuser");
        assert_eq!(perms, PERM_ALL);
    }

    #[test]
    fn test_resolve_permissions_unknown_user() {
        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![]),
            t_groups: Some(vec![]),
        };
        let perms = config.resolve_permissions("nobody");
        assert!(perms.is_empty());
    }

    #[test]
    fn test_resolve_permissions_case_insensitive_username() {
        let config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: Some(vec![WebUiUser {
                username: "Admin".to_string(),
                password_hash: "hash".to_string(),
                groups: vec!["admin".to_string()],
            }]),
            t_groups: Some(vec![]),
        };
        let perms = config.resolve_permissions("admin");
        assert_eq!(perms, PERM_ALL);
    }

    #[test]
    fn test_prepare_uses_default_group_file_without_double_prefix() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let config_path = tempdir.path().join("config");
        std::fs::create_dir_all(&config_path).expect("create config dir");

        let userfile_path = config_path.join("user.txt");
        std::fs::write(&userfile_path, "mod1:hash:mod\n").expect("write user file");

        let groupfile_path = config_path.join("groups.txt");
        std::fs::write(&groupfile_path, "mod:config.read\n").expect("write groups file");

        let mut config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: Some("user.txt".to_string()),
            groupfile: None,
            t_users: None,
            t_groups: None,
        };

        config.prepare(config_path.to_str().expect("config path utf-8")).expect("prepare config");

        let perms = config.resolve_permissions("mod1");
        assert!(perms.contains(Permission::ConfigRead));
    }

    #[test]
    fn test_prepare_treats_default_groupfile_string_as_default_path() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let workdir = tempdir.path().join("work");
        let config_path = workdir.join("config");
        std::fs::create_dir_all(&config_path).expect("create config dir");

        let userfile_path = config_path.join("user.txt");
        std::fs::write(&userfile_path, "mod1:hash:mod\n").expect("write user file");

        let groupfile_path = config_path.join("groups.txt");
        std::fs::write(&groupfile_path, "mod:config.read\n").expect("write groups file");

        let mut config = WebAuthConfig {
            enabled: true,
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: 60,
            userfile: Some("./config/user.txt".to_string()),
            groupfile: Some("./config/groups.txt".to_string()),
            t_users: None,
            t_groups: None,
        };

        let _guard = DirGuard::enter(&workdir).expect("enter workdir");
        let prepare_result = config.prepare(config_path.to_str().expect("config path utf-8"));

        prepare_result.expect("prepare config");

        let perms = config.resolve_permissions("mod1");
        assert!(perms.contains(Permission::ConfigRead));
    }

    // --- pwd_version_from_hash tests ---

    #[test]
    fn test_pwd_version_from_hash_deterministic() {
        let v1 = WebAuthConfig::pwd_version_from_hash("$argon2id$hash123");
        let v2 = WebAuthConfig::pwd_version_from_hash("$argon2id$hash123");
        assert_eq!(v1, v2);
    }

    #[test]
    fn test_pwd_version_from_hash_different_for_different_hashes() {
        let v1 = WebAuthConfig::pwd_version_from_hash("hash_a");
        let v2 = WebAuthConfig::pwd_version_from_hash("hash_b");
        assert_ne!(v1, v2);
    }

    #[test]
    fn test_web_ui_user_debug_redacts_password_hash() {
        let user = WebUiUser {
            username: "alice".to_string(),
            password_hash: "secret-hash".to_string(),
            groups: vec!["admin".to_string()],
        };

        let debug = format!("{user:?}");
        assert!(debug.contains("alice"));
        assert!(debug.contains("*****"));
        assert!(!debug.contains("secret-hash"));
    }
}
