use crate::{
    defaults::{default_auth_error_status, is_default_auth_error_status, is_false},
    error::TuliproxError,
    foundation::get_filter,
    model::{ClusterFlags, ProxyType, ProxyUserCredentialsDto},
    utils::{build_charset, generate_random_string_from, is_blank_optional_string, parse_duration_seconds},
};
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetUserDto {
    pub target: String,
    pub credentials: Vec<ProxyUserCredentialsDto>,
}

/// Trial settings for a plan: users created on the plan get an automatic
/// expiry when they don't bring their own `exp_date`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UserPlanTrialDto {
    /// Duration with unit, e.g. `24h`, `7d`.
    pub duration: String,
}

impl UserPlanTrialDto {
    pub fn duration_secs(&self) -> Option<u64> { parse_duration_seconds(&self.duration, true).filter(|secs| *secs > 0) }
}

/// Reusable capability tier referenced by users via `plan: <name>`.
/// User-level values always override plan values; unset user values inherit.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserPlanDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_clusters: Option<ClusterFlags>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyType>,
    #[serde(default)]
    pub max_connections: u32,
    #[serde(default)]
    pub soft_connections: u16,
    /// Filter DSL expression restricting visible content for plan members.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<UserPlanTrialDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub comment: Option<String>,
}

impl UserPlanDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("User plan name is empty".to_string()));
        }
        if let Some(filter) = &self.filter {
            let trimmed = filter.trim();
            if trimmed.is_empty() {
                self.filter = None;
            } else {
                // Templates are not available in api-proxy; fail fast on syntax errors.
                get_filter(trimmed, None).map_err(|err| {
                    TuliproxError::ConfigApiProxy(format!("Invalid filter in user plan {}: {err}", self.name))
                })?;
                self.filter = Some(trimmed.to_string());
            }
        }
        if let Some(trial) = &self.trial {
            if trial.duration_secs().is_none() {
                return Err(TuliproxError::ConfigApiProxy(format!(
                    "Invalid trial duration '{}' in user plan {} (expected e.g. 24h, 7d)",
                    trial.duration, self.name
                )));
            }
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiProxyServerInfoDto {
    pub name: String,
    pub protocol: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub port: Option<String>,
    pub timezone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub path: Option<String>,
}

pub const CREDENTIAL_LENGTH_MIN: u16 = 1;
pub const CREDENTIAL_LENGTH_MAX: u16 = 64;
pub const CREDENTIAL_DEFAULT_LENGTH: u16 = 6;
pub const CREDENTIAL_UPPER_CHARS: &str = "A-Z0-9";
pub const CREDENTIAL_MIXED_CHARS: &str = "A-Za-z0-9";

const fn default_credential_length() -> u16 { CREDENTIAL_DEFAULT_LENGTH }

/// One part of a generated credential: `length` chars picked from `chars` minus `exclude`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialSegmentDto {
    #[serde(default = "default_credential_length")]
    pub length: u16,
    /// Charset expression: literal chars and ranges `X-Y`; `\-` and `\\` escape literals.
    /// Only ASCII alphanumerics and `! . _ ~ -` are allowed. Empty means the default of the credential.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chars: String,
    /// Literal chars removed from `chars`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exclude: String,
}

impl Default for CredentialSegmentDto {
    fn default() -> Self { Self { length: CREDENTIAL_DEFAULT_LENGTH, chars: String::new(), exclude: String::new() } }
}

impl CredentialSegmentDto {
    fn charset(&self, default_chars: &str) -> Result<Vec<char>, String> {
        if !(CREDENTIAL_LENGTH_MIN..=CREDENTIAL_LENGTH_MAX).contains(&self.length) {
            return Err(format!(
                "length must be between {CREDENTIAL_LENGTH_MIN} and {CREDENTIAL_LENGTH_MAX}, got {}",
                self.length
            ));
        }
        let chars = if self.chars.trim().is_empty() { default_chars } else { self.chars.as_str() };
        build_charset(chars, &self.exclude)
    }
}

/// Generation rules for one auto-generated credential (username, password or token).
/// Either the flat form (`length`, `chars`, `exclude` describe one segment) or an
/// ordered `segments` list is used, never both.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialFieldGenDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chars: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exclude: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<CredentialSegmentDto>,
}

impl CredentialFieldGenDto {
    fn is_flat(&self) -> bool { self.length.is_some() || !self.chars.is_empty() || !self.exclude.is_empty() }

    /// The effective segments, whichever form is used.
    pub fn to_segments(&self) -> Vec<CredentialSegmentDto> {
        if self.segments.is_empty() {
            vec![CredentialSegmentDto {
                length: self.length.unwrap_or(CREDENTIAL_DEFAULT_LENGTH),
                chars: self.chars.clone(),
                exclude: self.exclude.clone(),
            }]
        } else {
            self.segments.clone()
        }
    }

    /// Builds the canonical form: a single segment is stored flat, defaults are omitted.
    pub fn from_segments(mut segments: Vec<CredentialSegmentDto>) -> Self {
        if segments.len() == 1 {
            let CredentialSegmentDto { length, chars, exclude } = segments.remove(0);
            Self {
                length: (length != CREDENTIAL_DEFAULT_LENGTH).then_some(length),
                chars,
                exclude,
                segments: Vec::new(),
            }
        } else {
            Self { segments, ..Self::default() }
        }
    }

    fn charsets(&self, default_chars: &str) -> Result<Vec<(Vec<char>, u16)>, String> {
        if !self.segments.is_empty() && self.is_flat() {
            return Err("use either length/chars/exclude or segments, not both".to_string());
        }
        let segments = self.to_segments();
        let mut total = 0u32;
        let mut result = Vec::with_capacity(segments.len());
        for (idx, segment) in segments.iter().enumerate() {
            let charset = segment.charset(default_chars).map_err(|err| {
                if segments.len() > 1 {
                    format!("segment {}: {err}", idx + 1)
                } else {
                    err
                }
            })?;
            total += u32::from(segment.length);
            result.push((charset, segment.length));
        }
        if total > u32::from(CREDENTIAL_LENGTH_MAX) {
            return Err(format!("total length must not exceed {CREDENTIAL_LENGTH_MAX}, got {total}"));
        }
        Ok(result)
    }

    pub fn validate(&self, default_chars: &str) -> Result<(), String> { self.charsets(default_chars).map(|_| ()) }

    /// Generates a value; falls back to a default 6 char value when the configuration is invalid.
    pub fn generate(&self, default_chars: &str) -> String {
        let parts = self.charsets(default_chars).unwrap_or_else(|_| {
            vec![(build_charset(default_chars, "").unwrap_or_default(), CREDENTIAL_DEFAULT_LENGTH)]
        });
        parts.iter().map(|(charset, length)| generate_random_string_from(charset, usize::from(*length))).collect()
    }
}

/// Rules for the credentials auto-filled when creating a new api user in the web UI.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialGenerationDto {
    #[serde(default, skip_serializing_if = "CredentialFieldGenDto::is_default")]
    pub username: CredentialFieldGenDto,
    #[serde(default, skip_serializing_if = "CredentialFieldGenDto::is_default")]
    pub password: CredentialFieldGenDto,
    #[serde(default, skip_serializing_if = "CredentialFieldGenDto::is_default")]
    pub token: CredentialFieldGenDto,
}

impl CredentialFieldGenDto {
    pub fn is_default(&self) -> bool { *self == Self::default() }
}

impl CredentialGenerationDto {
    pub fn is_default(&self) -> bool { *self == Self::default() }

    pub fn generate_username(&self) -> String { self.username.generate(CREDENTIAL_UPPER_CHARS) }
    pub fn generate_password(&self) -> String { self.password.generate(CREDENTIAL_UPPER_CHARS) }
    pub fn generate_token(&self) -> String { self.token.generate(CREDENTIAL_MIXED_CHARS) }

    /// Validation error per credential, `None` when valid.
    pub fn field_errors(&self) -> [Option<String>; 3] {
        [
            self.username.validate(CREDENTIAL_UPPER_CHARS).err(),
            self.password.validate(CREDENTIAL_UPPER_CHARS).err(),
            self.token.validate(CREDENTIAL_MIXED_CHARS).err(),
        ]
    }

    fn prepare(&self, errors: &mut Vec<String>) {
        for (name, err) in ["username", "password", "token"].into_iter().zip(self.field_errors()) {
            if let Some(err) = err {
                errors.push(format!("credential_generation.{name}: {err}"));
            }
        }
    }
}

fn is_default_credential_generation(value: &CredentialGenerationDto) -> bool { value.is_default() }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ApiProxyConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server: Vec<ApiProxyServerInfoDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<TargetUserDto>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_user_db: bool,
    /// HTTP status code returned for authentication failures (default: 403).
    #[serde(default = "default_auth_error_status", skip_serializing_if = "is_default_auth_error_status")]
    pub auth_error_status: u16,
    /// Length and charset of the credentials auto-generated for new users in the web UI.
    #[serde(default, skip_serializing_if = "is_default_credential_generation")]
    pub credential_generation: CredentialGenerationDto,
}

impl Default for ApiProxyConfigDto {
    fn default() -> Self {
        Self {
            server: Vec::new(),
            user: Vec::new(),
            use_user_db: false,
            auth_error_status: default_auth_error_status(),
            credential_generation: CredentialGenerationDto::default(),
        }
    }
}

impl ApiProxyServerInfoDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("Server info name is empty ".to_string()));
        }
        self.protocol = self.protocol.trim().to_string();
        if self.protocol.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("protocol can't be empty for api server config".to_string()));
        }
        self.host = self.host.trim().to_string();
        if self.host.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("host can't be empty for api server config".to_string()));
        }
        if let Some(port) = self.port.as_ref() {
            let port = port.trim().to_string();
            if port.is_empty() {
                self.port = None;
            } else if port.parse::<u16>().is_err() {
                return Err(TuliproxError::ConfigApiProxy("invalid port for api server config".to_string()));
            } else {
                self.port = Some(port);
            }
        }

        self.timezone = self.timezone.trim().to_string();
        if self.timezone.is_empty() {
            self.timezone = "UTC".to_string();
        }
        self.message = self.message.trim().to_string();
        if self.message.is_empty() {
            self.message = "Welcome to tuliprox".to_string();
        }
        if let Some(path) = &self.path {
            let trimmed_path = path.trim();
            if trimmed_path.is_empty() {
                self.path = None;
            } else {
                self.path = Some(trimmed_path.to_string());
            }
        }

        Ok(())
    }
    pub fn validate(&mut self) -> bool { self.prepare().is_ok() }
}

impl ApiProxyConfigDto {
    fn prepare_server_config(&mut self, errors: &mut Vec<String>) {
        let mut name_set = HashSet::new();
        for server in &mut self.server {
            if let Err(err) = server.prepare() {
                errors.push(err.to_string());
            }
            if name_set.contains(server.name.as_str()) {
                errors.push(format!("Non-unique server info name found {}", server.name));
            } else {
                name_set.insert(server.name.clone());
            }
        }
    }

    fn prepare_target_user(&mut self, errors: &mut Vec<String>, templates: Option<&[crate::model::PatternTemplate]>) {
        let mut usernames = HashSet::new();
        let mut tokens = HashSet::new();
        for target_user in &mut self.user {
            for user in &mut target_user.credentials {
                if let Err(err) = user.prepare_with_templates(templates) {
                    errors.push(err.to_string());
                }
                if usernames.contains(&user.username) {
                    errors.push(format!("Non unique username found {}", user.username));
                } else {
                    usernames.insert(user.username.clone());
                }
                if let Some(token) = &user.token {
                    if token.is_empty() {
                        user.token = None;
                    } else if tokens.contains(token) {
                        errors.push(format!(
                            "Non unique user token found {} for user {}",
                            user.token.as_ref().map_or_else(String::new, ToString::to_string),
                            user.username
                        ));
                    } else {
                        tokens.insert(token.clone());
                    }
                }

                if let Some(server_info_name) = &user.server {
                    if !&self.server.iter().any(|server_info| server_info.name.eq(server_info_name)) {
                        errors.push(format!(
                            "No server info with name {} found for user {}",
                            server_info_name, user.username
                        ));
                    }
                }
            }
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> { self.prepare_with_templates(None) }

    pub fn prepare_with_templates(
        &mut self,
        templates: Option<&[crate::model::PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let mut errors = Vec::new();
        if self.server.is_empty() {
            errors.push("No server info defined".to_string());
        } else {
            self.prepare_server_config(&mut errors);
        }
        self.prepare_target_user(&mut errors, templates);
        // A success or redirect code here would make auth failures look like valid responses
        if !(400..=599).contains(&self.auth_error_status) {
            errors.push(format!(
                "auth_error_status must be a client or server error code (400-599), got {}",
                self.auth_error_status
            ));
        }
        self.credential_generation.prepare(&mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(TuliproxError::ConfigApiProxy(errors.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_plan_dto_proxy_serde_roundtrip() {
        let plan = UserPlanDto {
            name: "test_plan".to_string(),
            proxy: Some(ProxyType::Reverse(Some(ClusterFlags::Live | ClusterFlags::Vod))),
            ..Default::default()
        };

        let serialized = serde_json::to_string(&plan).expect("should serialize");
        let deserialized: UserPlanDto = serde_json::from_str(&serialized).expect("should deserialize");
        assert_eq!(plan, deserialized);

        let plan_redirect =
            UserPlanDto { name: "redirect_plan".to_string(), proxy: Some(ProxyType::Redirect), ..Default::default() };
        let serialized_red = serde_json::to_string(&plan_redirect).expect("should serialize");
        let deserialized_red: UserPlanDto = serde_json::from_str(&serialized_red).expect("should deserialize");
        assert_eq!(plan_redirect, deserialized_red);

        let plan_none = UserPlanDto { name: "test_none".to_string(), proxy: None, ..Default::default() };
        let serialized_none = serde_json::to_string(&plan_none).expect("should serialize");
        assert!(!serialized_none.contains("\"proxy\""));
        let deserialized_none: UserPlanDto = serde_json::from_str(&serialized_none).expect("should deserialize");
        assert_eq!(plan_none, deserialized_none);
    }

    fn ok(field: &CredentialFieldGenDto) -> bool { field.validate("A-Z0-9").is_ok() }

    fn segment(length: u16, chars: &str) -> CredentialSegmentDto {
        CredentialSegmentDto { length, chars: chars.to_string(), exclude: String::new() }
    }

    #[test]
    fn credential_generation_defaults_are_not_serialized() {
        let dto = ApiProxyConfigDto::default();
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("credential_generation"));
        let parsed: ApiProxyConfigDto = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.credential_generation, CredentialGenerationDto::default());
    }

    #[test]
    fn credential_generation_roundtrip_flat_and_segments() {
        let json = r#"{"credential_generation":{"token":{"length":12,"chars":"a-z0-9","exclude":"l1"},
            "username":{"segments":[{"length":3,"chars":"A-Z"},{"length":3,"chars":"0-9"}]}}}"#;
        let dto: ApiProxyConfigDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.credential_generation.token.length, Some(12));
        assert_eq!(dto.credential_generation.username.segments.len(), 2);
        let back: ApiProxyConfigDto = serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert_eq!(dto, back);
    }

    #[test]
    fn credential_generation_missing_chars_uses_field_default() {
        let flat: CredentialFieldGenDto = serde_json::from_str(r#"{"length":8}"#).unwrap();
        assert!(flat.validate("A-Z0-9").is_ok());
        let value = flat.generate("0-1");
        assert_eq!(value.len(), 8);
        assert!(value.chars().all(|c| c == '0' || c == '1'));
        let seg: CredentialFieldGenDto =
            serde_json::from_str(r#"{"segments":[{"length":2},{"length":2,"chars":"x"}]}"#).unwrap();
        assert_eq!(&seg.generate("0")[..], "00xx");
    }

    #[test]
    fn credential_generation_segments_are_ordered() {
        let field = CredentialFieldGenDto::from_segments(vec![segment(3, "A-Z"), segment(3, "0-9")]);
        for _ in 0..20 {
            let value = field.generate("A-Z0-9");
            assert_eq!(value.len(), 6);
            assert!(value[..3].chars().all(|c| c.is_ascii_uppercase()), "{value}");
            assert!(value[3..].chars().all(|c| c.is_ascii_digit()), "{value}");
        }
    }

    #[test]
    fn credential_generation_validation() {
        // both forms given
        let both = CredentialFieldGenDto { length: Some(4), segments: vec![segment(2, "A")], ..Default::default() };
        assert!(both.validate("A-Z").unwrap_err().contains("not both"));
        // chars/exclude alone also count as flat
        let both =
            CredentialFieldGenDto { chars: "A".to_string(), segments: vec![segment(2, "A")], ..Default::default() };
        assert!(both.validate("A-Z").is_err());
        // length bounds, flat and per segment
        assert!(!ok(&CredentialFieldGenDto { length: Some(0), ..Default::default() }));
        assert!(!ok(&CredentialFieldGenDto { length: Some(65), ..Default::default() }));
        assert!(ok(&CredentialFieldGenDto { length: Some(64), ..Default::default() }));
        let err =
            CredentialFieldGenDto::from_segments(vec![segment(2, "A"), segment(0, "B")]).validate("A").unwrap_err();
        assert!(err.starts_with("segment 2"), "{err}");
        // total length
        assert!(!ok(&CredentialFieldGenDto::from_segments(vec![segment(40, "A"), segment(25, "B")])));
        assert!(ok(&CredentialFieldGenDto::from_segments(vec![segment(40, "A"), segment(24, "B")])));
        // invalid chars name the char, empty effective charset
        assert!(CredentialFieldGenDto { chars: "a b".to_string(), ..Default::default() }
            .validate("A")
            .unwrap_err()
            .contains("' '"));
        let empty = CredentialFieldGenDto { chars: "ab".to_string(), exclude: "ab".to_string(), ..Default::default() };
        assert!(!ok(&empty));
        // segments key present but empty means flat default
        let none: CredentialFieldGenDto = serde_json::from_str(r#"{"segments":[]}"#).unwrap();
        assert!(ok(&none));
    }

    #[test]
    fn credential_generation_prepare_reports_field_names() {
        let mut generation = CredentialGenerationDto::default();
        generation.token.length = Some(0);
        let mut errors = Vec::new();
        generation.prepare(&mut errors);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("credential_generation.token"));
    }

    #[test]
    fn credential_generation_from_segments_is_canonical() {
        assert!(CredentialFieldGenDto::from_segments(vec![CredentialSegmentDto::default()]).is_default());
        let single = CredentialFieldGenDto::from_segments(vec![segment(9, "a-f")]);
        assert_eq!(single.length, Some(9));
        assert_eq!(single.segments.len(), 0);
        assert_eq!(single.to_segments(), vec![segment(9, "a-f")]);
    }

    #[test]
    fn credential_generation_default_output_shape() {
        let generation = CredentialGenerationDto::default();
        let username = generation.generate_username();
        assert_eq!(username.len(), 6);
        assert!(username.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        let token = generation.generate_token();
        assert_eq!(token.len(), 6);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn invalid_credential_generation_falls_back_to_default() {
        let bad = CredentialFieldGenDto { length: Some(0), chars: "?".to_string(), ..Default::default() };
        assert_eq!(bad.generate("A-Z0-9").len(), 6);
    }
}
