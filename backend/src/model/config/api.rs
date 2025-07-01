use shared::model::ConfigApiDto;
use crate::model::macros;

#[derive(Debug, Clone, Default)]
pub struct ConfigApi {
    pub host: String,
    pub port: u16,
    pub web_root: String,
}

macros::from_impl!(ConfigApi);
impl From<&ConfigApiDto> for ConfigApi {
    fn from(dto: &ConfigApiDto) -> Self {
        Self {
            host:dto.host.to_string(),
            port: dto.port,
            web_root: dto.web_root.to_string(),
        }
    }
}

impl From<&ConfigApi> for ConfigApiDto {
    fn from(instance: &ConfigApi) -> Self {
        Self {
            host: instance.host.to_string(),
            port: instance.port,
            web_root: instance.web_root.to_string(),
        }
    }
}