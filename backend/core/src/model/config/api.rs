use crate::model::{macros, ApiProxyServerInfo};
use shared::model::ConfigApiDto;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Default)]
pub struct ConfigApi {
    pub host: String,
    pub port: u16,
    pub web_root: String,
}

macros::from_impl!(ConfigApi);
impl From<&ConfigApiDto> for ConfigApi {
    fn from(dto: &ConfigApiDto) -> Self {
        Self { host: dto.host.clone(), port: dto.port, web_root: dto.web_root.clone() }
    }
}

impl From<&ConfigApi> for ConfigApiDto {
    fn from(instance: &ConfigApi) -> Self {
        Self { host: instance.host.clone(), port: instance.port, web_root: instance.web_root.clone() }
    }
}

impl ConfigApi {
    /// This process's own API listener as a playback server. Internal clients
    /// reach it without the public address, which may need external DNS, TLS
    /// or a reverse proxy. A wildcard bind address is reached via loopback.
    pub fn local_server_info(&self) -> ApiProxyServerInfo {
        let host = self.host.trim().trim_matches(['[', ']']);
        let host = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
            Ok(IpAddr::V6(ip)) => format!("[{}]", if ip.is_unspecified() { Ipv6Addr::LOCALHOST } else { ip }),
            _ => host.to_string(),
        };
        ApiProxyServerInfo {
            name: "local".to_string(),
            protocol: "http".to_string(),
            host,
            port: Some(self.port.to_string()),
            timezone: "UTC".to_string(),
            message: String::new(),
            path: None,
        }
    }
}
