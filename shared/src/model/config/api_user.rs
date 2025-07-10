use crate::utils::default_as_true;
use std::fmt::Display;
use std::str::FromStr;
use enum_iterator::Sequence;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use crate::create_tuliprox_error_result;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{ClusterFlags, PlaylistItemType};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum UserConnectionPermission {
    Exhausted,
    Allowed,
    GracePeriod,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProxyType {
    Reverse(Option<ClusterFlags>),
    Redirect,
}

impl Default for ProxyType {
    fn default() -> Self {
        Self::Redirect
    }
}

impl ProxyType {
    const REVERSE: &'static str = "reverse";
    const REDIRECT: &'static str = "redirect";

    pub fn is_redirect(&self, item_type: PlaylistItemType) -> bool {
        match self {
            ProxyType::Reverse(Some(flags)) => {
                if flags.is_empty() {
                    return false;
                }
                if flags.has_cluster(item_type) {
                    return false;
                }
                true
            },
            ProxyType::Reverse(None) => false,
            ProxyType::Redirect => true
        }
    }

    pub fn is_reverse(&self, item_type: PlaylistItemType) -> bool {
        !self.is_redirect(item_type)
    }
}

impl Display for ProxyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reverse(force_redirect) => {
                if let Some(force) = force_redirect {
                    write!(f, "{}{force:?}", Self::REVERSE)
                } else {
                    write!(f, "{}", Self::REVERSE)
                }
            }
            Self::Redirect => write!(f, "{}", Self::REDIRECT),
        }
    }
}

impl FromStr for ProxyType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s == Self::REDIRECT {
            return Ok(Self::Redirect);
        }
        if s == Self::REVERSE {
            return Ok(Self::Reverse(None));
        }

        if s.starts_with(Self::REVERSE) {
            let suffix = s.strip_prefix(Self::REVERSE).unwrap();
            if let Ok(force_redirect) = ClusterFlags::try_from(suffix) {
                if force_redirect.has_full_flags() {
                    return Ok(ProxyType::Reverse(None));
                }
                return Ok(Self::Reverse(Some(force_redirect)));
            }
        }

        create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown ProxyType: {}", s)
    }
}

impl<'de> Deserialize<'de> for ProxyType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: String = Deserialize::deserialize(deserializer)?;
        if raw == ProxyType::REDIRECT {
            return Ok(ProxyType::Redirect);
        } else if raw.starts_with(ProxyType::REVERSE) {
            return ProxyType::from_str(raw.as_str()).map_err(serde::de::Error::custom);
        }
        Err(serde::de::Error::custom("Unknown proxy type"))
    }
}

impl Serialize for ProxyType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            ProxyType::Redirect => serializer.serialize_str(ProxyType::REDIRECT),
            ProxyType::Reverse(None) => serializer.serialize_str(ProxyType::REVERSE),
            ProxyType::Reverse(Some(ref force_redirect)) => {
                serializer.serialize_str(&format!("{}{}", ProxyType::REVERSE, force_redirect))
            },
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq)]
pub enum ProxyUserStatus {
    Active, // The account is in good standing and can stream content
    Expired, // The account can no longer access content unless it is renewed.
    Banned, // The account is temporarily or permanently disabled. Typically used for users who violate terms of service or abuse the system.
    Trial, // The account is marked as a trial account.
    Disabled, // The account is inactive or deliberately disabled by the administrator.
    Pending,
}


impl Default for ProxyUserStatus {
    fn default() -> Self {
        Self::Active
    }
}

impl ProxyUserStatus {
    const ACTIVE: &'static str = "Active";
    const EXPIRED: &'static str = "Expired";
    const BANNED: &'static str = "Banned";
    const TRIAL: &'static str = "Trial";
    const DISABLED: &'static str = "Disabled";
    const PENDING: &'static str = "Pending";
}

impl Display for ProxyUserStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Active => Self::ACTIVE,
            Self::Expired => Self::EXPIRED,
            Self::Banned => Self::BANNED,
            Self::Trial => Self::TRIAL,
            Self::Disabled => Self::DISABLED,
            Self::Pending => Self::PENDING,
        })
    }
}

impl FromStr for ProxyUserStatus {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::ACTIVE => Ok(Self::Active),
            Self::EXPIRED => Ok(Self::Expired),
            Self::BANNED => Ok(Self::Banned),
            Self::TRIAL => Ok(Self::Trial),
            Self::DISABLED => Ok(Self::Disabled),
            Self::PENDING => Ok(Self::Pending),
            _ => create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown ProxyUserStatus: {}", s)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyUserCredentialsDto {
    pub username: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "ProxyType::default")]
    pub proxy: ProxyType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epg_timeshift: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default)]
    pub max_connections: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ProxyUserStatus>,
    #[serde(default = "default_as_true")]
    pub ui_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}


impl ProxyUserCredentialsDto {
    pub fn prepare(&mut self) {
        self.trim();
    }

    fn trim(&mut self) {
        self.username = self.username.trim().to_string();
        self.password = self.password.trim().to_string();
        match &self.token {
            None => {}
            Some(tkn) => {
                self.token = Some(tkn.trim().to_string());
            }
        }
    }

    pub fn validate(&self) -> Result<(), TuliproxError> {
        if self.username.is_empty() {
            return Err(TuliproxError::new(TuliproxErrorKind::Info, "Username required".to_string()));
        }
        if self.password.is_empty() {
            return Err(TuliproxError::new(TuliproxErrorKind::Info, "Password required".to_string()));
        }
        Ok(())
    }

}