use std::fs::File;
use std::io::BufRead;
use std::path::PathBuf;
use shared::error::{TuliproxError, TuliproxErrorKind, create_tuliprox_error_result};
use shared::model::{UserCredential, WebAuthConfigDto};
use crate::model::macros;
use crate::utils;

#[derive(Debug, Clone)]
pub struct WebAuthConfig {
    pub enabled: bool,
    pub issuer: String,
    pub secret: String,
    pub userfile: Option<String>,
    pub t_users: Option<Vec<UserCredential>>,
}

macros::from_impl!(WebAuthConfig);
impl From<&WebAuthConfigDto> for WebAuthConfig {
    fn from(dto: &WebAuthConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            issuer: dto.issuer.to_string(),
            secret: dto.secret.to_string(),
            userfile: dto.userfile.clone(),
            t_users: None,
        }
    }
}

impl From<&WebAuthConfig> for WebAuthConfigDto {
    fn from(instance: &WebAuthConfig) -> Self {
        Self {
            enabled: instance.enabled,
            issuer: instance.issuer.to_string(),
            secret: instance.secret.to_string(),
            userfile: instance.userfile.clone(),
        }
    }
}

impl WebAuthConfig {
    pub fn prepare(&mut self, config_path: &str) -> Result<(), TuliproxError> {
        let userfile_name = self.userfile.as_ref().map_or_else(|| utils::get_default_user_file_path(config_path), std::borrow::ToOwned::to_owned);
        self.userfile = Some(userfile_name.clone());

        let mut userfile_path = PathBuf::from(&userfile_name);
        if !utils::path_exists(&userfile_path) {
            userfile_path = PathBuf::from(config_path).join(&userfile_name);
            if !utils::path_exists(&userfile_path) {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Could not find userfile {}", &userfile_name);
            }
        }

        if let Ok(file) = File::open(&userfile_path) {
            let mut users = vec![];
            let reader = utils::file_reader(file);
            // TODO maybe print out errors
            for credentials in reader.lines().map_while(Result::ok) {
                let mut parts = credentials.split(':');
                if let (Some(username), Some(password)) = (parts.next(), parts.next()) {
                    users.push(UserCredential {
                        username: username.trim().to_string(),
                        password: password.trim().to_string(),
                    });
                    // debug!("Read ui user {}", username);
                }
            }

            self.t_users = Some(users);
        } else {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Could not read userfile {:?}", &userfile_path);
        }
        Ok(())
    }

    pub fn get_user_password(&self, username: &str) -> Option<&str> {
        if let Some(users) = &self.t_users {
            for credential in users {
                if credential.username.eq_ignore_ascii_case(username) {
                    return Some(credential.password.as_str());
                }
            }
        }
        None
    }
}