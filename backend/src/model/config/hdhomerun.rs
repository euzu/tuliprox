use shared::model::{HdHomeRunConfigDto, HdHomeRunDeviceConfigDto};
use crate::model::macros;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HdHomeRunDeviceConfig {
    pub friendly_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub model_number: String,
    pub firmware_name: String,
    pub firmware_version: String,
    // pub device_auth: String,
    pub device_type: String,
    pub device_udn: String,
    pub name: String,
    pub port: u16,
    pub tuner_count: u8,
    pub t_username: String,
    pub t_enabled: bool,
}

macros::from_impl!(HdHomeRunDeviceConfig);
impl From<&HdHomeRunDeviceConfigDto> for HdHomeRunDeviceConfig {
    fn from(dto: &HdHomeRunDeviceConfigDto) -> Self {
        Self {
            friendly_name: dto.friendly_name.to_string(),
            manufacturer: dto.manufacturer.to_string(),
            model_name: dto.model_name.to_string(),
            model_number: dto.model_number.to_string(),
            firmware_name: dto.firmware_name.to_string(),
            firmware_version: dto.firmware_version.to_string(),
            device_type: dto.device_type.to_string(),
            device_udn: dto.device_udn.to_string(),
            name: dto.name.to_string(),
            port: dto.port,
            tuner_count: dto.tuner_count,
            t_username: String::new(),
            t_enabled: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HdHomeRunConfig {
    pub enabled: bool,
    pub auth: bool,
    pub devices: Vec<HdHomeRunDeviceConfig>,
}

macros::from_impl!(HdHomeRunConfig);
impl From<&HdHomeRunConfigDto> for HdHomeRunConfig {
    fn from(dto: &HdHomeRunConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            auth: dto.auth,
            devices: dto.devices.iter().map(Into::into).collect(),
        }
    }
}
