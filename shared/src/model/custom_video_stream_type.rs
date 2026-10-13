use crate::error::TuliproxError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

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
    InputMaintenance,
}

impl fmt::Display for CustomVideoStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CustomVideoStreamType::ChannelUnavailable => "channel_unavailable",
            CustomVideoStreamType::UserConnectionsExhausted => "user_connections_exhausted",
            CustomVideoStreamType::ProviderConnectionsExhausted => "provider_connections_exhausted",
            CustomVideoStreamType::LowPriorityPreempted => "low_priority_preempted",
            CustomVideoStreamType::UserAccountExpired => "user_account_expired",
            CustomVideoStreamType::Provisioning => "provisioning",
            CustomVideoStreamType::HlsSessionOrLeaseExpired => "hls_session_or_lease_expired",
            CustomVideoStreamType::InputMaintenance => "input_maintenance",
        };
        write!(f, "{s}")
    }
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
            "input_maintenance" => Ok(Self::InputMaintenance),
            _ => Err(TuliproxError::Config(format!("Unknown stream type: {s}"))),
        }
    }
}

impl Serialize for CustomVideoStreamType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
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
    fn input_maintenance_round_trips() {
        let parsed = CustomVideoStreamType::from_str("input_maintenance").unwrap();
        assert_eq!(parsed, CustomVideoStreamType::InputMaintenance);
        assert_eq!(parsed.to_string(), "input_maintenance");
    }
}
