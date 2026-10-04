use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

#[derive(
    Debug,
    Default,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Ord,
    PartialOrd,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum ProxyUserStatus {
    #[default]
    Active, // The account is in good standing and can stream content
    Expired,  // The account can no longer access content unless it is renewed.
    Banned, // The account is temporarily or permanently disabled. Typically used for users who violate terms of service or abuse the system.
    Trial,  // The account is marked as a trial account.
    Disabled, // The account is inactive or deliberately disabled by the administrator.
    Pending,
}

impl ProxyUserStatus {
    /// Accounts in these states can serve streams.
    pub const fn is_usable(self) -> bool { matches!(self, Self::Active | Self::Trial) }

    /// Confirmed states that justify a persisted exclusion; `Pending` stays runtime-only.
    pub const fn is_terminal(self) -> bool { matches!(self, Self::Banned | Self::Disabled | Self::Expired) }
}

/// Stable fingerprint of a provider account (URL and credentials) without keeping secrets around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderAccountIdentity(blake3::Hash);

impl ProviderAccountIdentity {
    pub fn new(url: &str, username: Option<&str>, password: Option<&str>) -> Self {
        let mut hash = blake3::Hasher::new();
        for value in [url, username.unwrap_or_default(), password.unwrap_or_default()] {
            hash.update(value.as_bytes());
            hash.update(b"\0");
        }
        Self(hash.finalize())
    }
}

impl std::fmt::Display for ProviderAccountIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0.to_hex()) }
}
