use crate::error::TuliproxError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

pub const HEADER_TULIPROX_FALLBACK: &str = "x-tuliprox-fallback";
pub const HEADER_TULIPROX_REJECTION: &str = "x-tuliprox-rejection";
pub const HEADER_TULIPROX_STREAM_UID: &str = "x-tuliprox-stream-uid";
pub const HEADER_TULIPROX_PLAYER_API_PATH: &str = "x-tuliprox-player-api-path";

/// The reason a request is answered with a canned video clip instead of the
/// provider stream the caller asked for.
///
/// This is vocabulary shared between the code that *decides* a substitution is
/// needed (admission, provider selection, session accounting) and the code that
/// *performs* it, so it lives here rather than beside either one.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
pub enum CustomVideoStreamType {
    ChannelUnavailable,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    LowPriorityPreempted,
    UserAccountExpired,
    Provisioning,
    HlsSessionOrLeaseExpired,
    Maintenance,
}

impl CustomVideoStreamType {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ChannelUnavailable => "channel_unavailable",
            Self::UserConnectionsExhausted => "user_connections_exhausted",
            Self::ProviderConnectionsExhausted => "provider_connections_exhausted",
            Self::LowPriorityPreempted => "low_priority_preempted",
            Self::UserAccountExpired => "user_account_expired",
            Self::Provisioning => "provisioning",
            Self::HlsSessionOrLeaseExpired => "hls_session_or_lease_expired",
            Self::Maintenance => "maintenance",
        }
    }
}

impl fmt::Display for CustomVideoStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}

impl FromStr for CustomVideoStreamType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "channel_unavailable" => Ok(Self::ChannelUnavailable),
            "user_connections_exhausted" => Ok(Self::UserConnectionsExhausted),
            "provider_connections_exhausted" => Ok(Self::ProviderConnectionsExhausted),
            "low_priority_preempted" => Ok(Self::LowPriorityPreempted),
            "user_account_expired" => Ok(Self::UserAccountExpired),
            "provisioning" => Ok(Self::Provisioning),
            "hls_session_or_lease_expired" => Ok(Self::HlsSessionOrLeaseExpired),
            "maintenance" => Ok(Self::Maintenance),
            _ => Err(TuliproxError::Config(format!("Unknown stream type: {s}"))),
        }
    }
}

impl Serialize for CustomVideoStreamType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for CustomVideoStreamType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::CustomVideoStreamType;
    use std::str::FromStr;

    #[test]
    fn maintenance_round_trips() {
        let parsed = CustomVideoStreamType::from_str("maintenance").unwrap();
        assert_eq!(parsed, CustomVideoStreamType::Maintenance);
        assert_eq!(parsed.to_string(), "maintenance");
    }
}
