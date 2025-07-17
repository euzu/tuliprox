use shared::model::ProxyConfigDto;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

macros::from_impl!(ProxyConfig);
impl From<&ProxyConfigDto>  for ProxyConfig {
    fn from(dto: &ProxyConfigDto) -> Self {
        Self {
            url: dto.url.to_string(),
            username: dto.username.clone(),
            password: dto.password.clone(),
        }
    }
}

impl From<&ProxyConfig>  for ProxyConfigDto {
    fn from(dto: &ProxyConfig) -> Self {
        Self {
            url: dto.url.to_string(),
            username: dto.username.clone(),
            password: dto.password.clone(),
        }
    }
}
