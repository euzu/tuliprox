use crate::{
    error::TuliproxError,
    model::{ClusterFlags, ProxyType, ProxyUserStatus, XtreamCluster},
    utils::{
        default_as_true, default_user_priority, deserialize_timestamp, is_blank_optional_string,
        is_default_user_priority, is_true,
    },
};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum UserConnectionPermission {
    Exhausted,
    Allowed,
    GracePeriod,
}

fn default_output_clusters() -> ClusterFlags { ClusterFlags::all() }

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
    #[serde(default = "default_output_clusters")]
    pub output_clusters: ClusterFlags,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub ui_enabled: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub comment: Option<String>,
    #[serde(default = "default_user_priority", skip_serializing_if = "is_default_user_priority")]
    pub priority: i8,
    #[serde(default)]
    pub soft_connections: u16,
    #[serde(default = "default_user_priority", skip_serializing_if = "is_default_user_priority")]
    pub soft_priority: i8,
}

impl ProxyUserCredentialsDto {
    pub fn prepare(&mut self) { self.trim(); }

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
        match cluster {
            XtreamCluster::Live => self.output_clusters.contains(ClusterFlags::Live),
            XtreamCluster::Video => self.output_clusters.contains(ClusterFlags::Vod),
            XtreamCluster::Series => self.output_clusters.contains(ClusterFlags::Series),
        }
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
            output_clusters: default_output_clusters(),
            ui_enabled: default_as_true(),
            comment: None,
            priority: default_user_priority(),
            soft_connections: 0,
            soft_priority: default_user_priority(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_user_credentials_defaults_output_clusters_to_all_when_missing() {
        let value = serde_json::json!({
            "username": "alice",
            "password": "secret"
        });

        let user: ProxyUserCredentialsDto = serde_json::from_value(value).expect("user should deserialize");

        assert_eq!(user.output_clusters, ClusterFlags::all());
    }

    #[test]
    fn proxy_user_credentials_roundtrip_preserves_output_clusters() {
        let user = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "secret".to_string(),
            output_clusters: ClusterFlags::Live | ClusterFlags::Series,
            ..Default::default()
        };

        let serialized = serde_json::to_value(&user).expect("user should serialize");
        let deserialized: ProxyUserCredentialsDto =
            serde_json::from_value(serialized).expect("user should deserialize");

        assert_eq!(deserialized.output_clusters, ClusterFlags::Live | ClusterFlags::Series);
    }
}
