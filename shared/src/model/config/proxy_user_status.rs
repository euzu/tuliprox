use std::fmt::Display;
use std::str::FromStr;
use enum_iterator::Sequence;
use crate::info_err_res;
use crate::error::{TuliproxError};

#[derive(Debug, Default, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Ord, PartialOrd)]
pub enum ProxyUserStatus {
    #[default]
    Active, // The account is in good standing and can stream content
    Expired, // The account can no longer access content unless it is renewed.
    Banned, // The account is temporarily or permanently disabled. Typically used for users who violate terms of service or abuse the system.
    Trial, // The account is marked as a trial account.
    Disabled, // The account is inactive or deliberately disabled by the administrator.
    Pending,
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
            _ => info_err_res!("Unknown ProxyUserStatus: {}", s)
        }
    }
}
