use crate::{
    defaults::{
        default_as_true, default_user_priority, is_cluster_optional, is_default_user_priority, is_false, is_true,
    },
    error::TuliproxError,
    model::{ClusterFlags, NetworkAccessDto, ProxyType, ProxyUserStatus, XtreamCluster},
    utils::{deserialize_timestamp, is_blank_optional_string},
};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum UserConnectionPermission {
    Exhausted,
    Allowed,
    GracePeriod,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyUserCredentialsDto {
    pub username: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub token: Option<String>,
    #[serde(default = "ProxyType::default")]
    pub proxy: ProxyType,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub epg_timeshift: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub epg_request_timeshift: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default)]
    pub max_connections: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProxyUserStatus>,
    #[serde(default, skip_serializing_if = "is_cluster_optional")]
    pub output_clusters: Option<ClusterFlags>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub ui_enabled: bool,
    /// When true, Adult / 18+ / XXX categories and channels are omitted from playlists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_adult: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub comment: Option<String>,
    #[serde(default = "default_user_priority", skip_serializing_if = "is_default_user_priority")]
    pub priority: i8,
    #[serde(default)]
    pub soft_connections: u16,
    #[serde(default = "default_user_priority", skip_serializing_if = "is_default_user_priority")]
    pub soft_priority: i8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<NetworkAccessDto>,
}

impl ProxyUserCredentialsDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.trim();
        if let Some(na) = &mut self.network_access {
            na.prepare()?;
            if na.is_empty() {
                self.network_access = None;
            }
        }
        Ok(())
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
            return Err(TuliproxError::ProxyUser("Username required".to_string()));
        }
        if self.password.is_empty() {
            return Err(TuliproxError::ProxyUser("Password required".to_string()));
        }
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        if let Some(status) = &self.status {
            if matches!(
                status,
                ProxyUserStatus::Expired
                    | ProxyUserStatus::Banned
                    | ProxyUserStatus::Disabled
                    | ProxyUserStatus::Pending
            ) {
                return false;
            }
        }
        if let Some(exp_date) = self.exp_date {
            let now = chrono::Utc::now().timestamp();
            if exp_date < now {
                return false;
            }
        }
        true
    }

    pub fn allows_cluster(&self, cluster: XtreamCluster) -> bool {
        self.output_clusters.as_ref().is_none_or(|output_clusters| match cluster {
            XtreamCluster::Live => output_clusters.contains(ClusterFlags::Live),
            XtreamCluster::Video => output_clusters.contains(ClusterFlags::Vod),
            XtreamCluster::Series => output_clusters.contains(ClusterFlags::Series),
        })
    }
}

impl Default for ProxyUserCredentialsDto {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            token: None,
            proxy: ProxyType::default(),
            server: None,
            epg_timeshift: None,
            epg_request_timeshift: None,
            created_at: None,
            exp_date: None,
            max_connections: 0,
            status: None,
            output_clusters: None,
            ui_enabled: default_as_true(),
            hide_adult: false,
            comment: None,
            priority: default_user_priority(),
            soft_connections: 0,
            soft_priority: default_user_priority(),
            network_access: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_user_credentials_output_clusters_none_preserved() {
        let value = serde_json::json!({
            "username": "alice",
            "password": "secret"
        });

        let user: ProxyUserCredentialsDto = serde_json::from_value(value).expect("user should deserialize");

        assert_eq!(user.output_clusters, None);
    }

    #[test]
    fn proxy_user_credentials_roundtrip_preserves_output_clusters() {
        let user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            output_clusters: Some(ClusterFlags::Live | ClusterFlags::Series),
            ..ProxyUserCredentialsDto::default()
        };

        let serialized = serde_json::to_value(&user).expect("user should serialize");
        let deserialized: ProxyUserCredentialsDto =
            serde_json::from_value(serialized).expect("user should deserialize");

        assert_eq!(deserialized.output_clusters, Some(ClusterFlags::Live | ClusterFlags::Series));
    }

    #[test]
    fn proxy_user_credentials_roundtrip_preserves_empty_output_clusters() {
        let user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            output_clusters: Some(ClusterFlags::empty()),
            ..ProxyUserCredentialsDto::default()
        };

        let serialized = serde_json::to_value(&user).expect("user should serialize");
        let deserialized: ProxyUserCredentialsDto =
            serde_json::from_value(serialized).expect("user should deserialize");

        assert_eq!(deserialized.output_clusters, Some(ClusterFlags::empty()));
    }

    #[test]
    fn prepare_rejects_invalid_country_in_network_access() {
        let mut user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            network_access: Some(NetworkAccessDto {
                allowed_countries: Some(vec!["INVALID".to_string()]),
                allowed_networks: None,
            }),
            ..Default::default()
        };
        assert!(user.prepare().is_err());
    }

    #[test]
    fn prepare_rejects_invalid_cidr_in_network_access() {
        let mut user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            network_access: Some(NetworkAccessDto {
                allowed_countries: None,
                allowed_networks: Some(vec!["not-a-cidr".to_string()]),
            }),
            ..Default::default()
        };
        assert!(user.prepare().is_err());
    }

    #[test]
    fn prepare_rejects_cidr_with_extra_segment() {
        let mut user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            network_access: Some(NetworkAccessDto {
                allowed_countries: None,
                allowed_networks: Some(vec!["10.0.0.0/8/extra".to_string()]),
            }),
            ..Default::default()
        };
        assert!(user.prepare().is_err());
    }

    #[test]
    fn prepare_normalizes_empty_network_access_to_none() {
        let mut user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            network_access: Some(NetworkAccessDto { allowed_countries: Some(vec![]), allowed_networks: Some(vec![]) }),
            ..Default::default()
        };
        user.prepare().unwrap();
        assert_eq!(user.network_access, None);
    }

    #[test]
    fn prepare_normalizes_and_deduplicates_network_access() {
        let mut user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            network_access: Some(NetworkAccessDto {
                allowed_countries: Some(vec!["de".to_string(), "DE".to_string(), "at".to_string()]),
                allowed_networks: Some(vec![
                    "10.0.0.1/8".to_string(), // non-canonical, normalized to 10.0.0.0/8
                    "10.0.0.0/8".to_string(), // duplicate after normalization
                    "192.168.1.0/24".to_string(),
                ]),
            }),
            ..Default::default()
        };
        user.prepare().unwrap();
        let na = user.network_access.as_ref();
        assert!(na.is_some());
        if let Some(na) = na {
            assert_eq!(na.allowed_countries, Some(vec!["DE".to_string(), "AT".to_string()]));
            assert_eq!(na.allowed_networks.as_ref().map(|n| n.len()), Some(2));
            assert!(na.allowed_networks.as_ref().is_some_and(|networks| networks.iter().any(|n| n == "10.0.0.0/8")));
            assert!(na
                .allowed_networks
                .as_ref()
                .is_some_and(|networks| networks.iter().any(|n| n == "192.168.1.0/24")));
        }
    }

    #[test]
    fn proxy_user_credentials_roundtrip_preserves_hide_adult() {
        let user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            hide_adult: true,
            ..ProxyUserCredentialsDto::default()
        };

        let serialized = serde_json::to_value(&user).expect("user should serialize");
        let deserialized: ProxyUserCredentialsDto =
            serde_json::from_value(serialized).expect("user should deserialize");

        assert!(deserialized.hide_adult);
    }

    #[test]
    fn proxy_user_credentials_default_hides_adult_false() {
        let value = serde_json::json!({
            "username": "alice",
            "password": "secret"
        });

        let user: ProxyUserCredentialsDto = serde_json::from_value(value).expect("user should deserialize");
        assert!(!user.hide_adult);
    }
}
