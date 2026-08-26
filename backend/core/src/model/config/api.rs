use crate::model::macros;
use shared::model::ConfigApiDto;

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
