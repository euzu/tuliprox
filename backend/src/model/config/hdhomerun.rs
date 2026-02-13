use shared::model::{HdHomeRunConfigDto, HdHomeRunDeviceConfigDto};
use crate::model::macros;
use shared::create_bitset;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HdHomeRunDeviceConfig {
    pub friendly_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub model_number: String,
    pub firmware_name: String,
    pub firmware_version: String,
    pub device_id: String,
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
            friendly_name: dto.friendly_name.clone(),
            manufacturer: dto.manufacturer.clone(),
            model_name: dto.model_name.clone(),
            model_number: dto.model_number.clone(),
            firmware_name: dto.firmware_name.clone(),
            firmware_version: dto.firmware_version.clone(),
            device_id: dto.device_id.clone(),
            device_type: dto.device_type.clone(),
            device_udn: dto.device_udn.clone(),
            name: dto.name.clone(),
            port: dto.port,
            tuner_count: dto.tuner_count,
            t_username: String::new(),
            t_enabled: false,
        }
    }
}

create_bitset!(
    u8,
    HdHomeRunFlags,
    Enabled,
    Auth,
    SsdpDiscovery,
    ProprietaryDiscovery
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HdHomeRunConfig {
    pub flags: HdHomeRunFlagsSet,
    pub devices: Vec<HdHomeRunDeviceConfig>,
}

macros::from_impl!(HdHomeRunConfig);
impl From<&HdHomeRunConfigDto> for HdHomeRunConfig {
    fn from(dto: &HdHomeRunConfigDto) -> Self {
        let mut flags = HdHomeRunFlagsSet::new();
        if dto.enabled {
            flags.add(HdHomeRunFlags::Enabled);
        }
        if dto.auth {
            flags.add(HdHomeRunFlags::Auth);
        }
        if dto.ssdp_discovery {
            flags.add(HdHomeRunFlags::SsdpDiscovery);
        }
        if dto.proprietary_discovery {
            flags.add(HdHomeRunFlags::ProprietaryDiscovery);
        }
        Self {
            flags,
            devices: dto.devices.iter().map(Into::into).collect(),
        }
    }
}
