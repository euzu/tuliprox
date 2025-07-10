use std::collections::HashSet;
use enum_iterator::Sequence;
use log::warn;
use crate::create_tuliprox_error_result;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::TargetType;

fn default_friendly_name() -> String { String::from("TuliproxTV") }
fn default_manufacturer() -> String { String::from("Silicondust") }
fn default_model_name() -> String { String::from("HDTC-2US") }
fn default_firmware_name() -> String { String::from("hdhomeruntc_atsc") }
fn default_firmware_version() -> String { String::from("20170930") }
fn default_device_type() -> String { String::from("urn:schemas-upnp-org:device:MediaServer:1") }
fn default_device_udn() -> String { String::from("uuid:12345678-90ab-cdef-1234-567890abcdef::urn:dial-multicast:com.silicondust.hdhomerun") }


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Hash)]
enum HdHomeRunUseTargetType {
    #[serde(rename = "m3u")]
    M3u,
    #[serde(rename = "xtream")]
    Xtream,
}

impl TryFrom<TargetType> for HdHomeRunUseTargetType {
    type Error = &'static str;

    fn try_from(value: TargetType) -> Result<Self, Self::Error> {
        match value {
            TargetType::Xtream => Ok(Self::Xtream),
            TargetType::M3u => Ok(Self::M3u),
            _ => Err("Not allowed!"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunDeviceConfigDto {
    #[serde(default = "default_friendly_name")]
    pub friendly_name: String,
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_model_name")]
    pub model_name: String,
    #[serde(default = "default_model_name")]
    pub model_number: String,
    #[serde(default = "default_firmware_name")]
    pub firmware_name: String,
    #[serde(default = "default_firmware_version")]
    pub firmware_version: String,
    // pub device_auth: String,
    #[serde(default = "default_device_type")]
    pub device_type: String,
    #[serde(default = "default_device_udn")]
    pub device_udn: String,
    pub name: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub tuner_count: u8,
}

impl HdHomeRunDeviceConfigDto {
    pub fn prepare(&mut self, device_num: u8) {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = format!("device{device_num}");
            warn!("Device name empty, assigned new name: {}", self.name);
        }

        if self.tuner_count == 0 {
            self.tuner_count = 1;
        }

        if device_num > 0 && self.friendly_name == default_friendly_name() {
            self.friendly_name = format!("{} {}", self.friendly_name, device_num);
        }
        if self.device_udn == default_device_udn() {
            self.device_udn = format!("{}:{}", self.device_udn, device_num+1);
        }
    }
}


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auth: bool,
    pub devices: Vec<HdHomeRunDeviceConfigDto>,
}

impl HdHomeRunConfigDto {
    pub fn prepare(&mut self, api_port: u16)  -> Result<(), TuliproxError> {
        let mut names = HashSet::new();
        let mut ports = HashSet::new();
        ports.insert(api_port);
        for (device_num, device) in (0_u8..).zip(self.devices.iter_mut()) {
            device.prepare(device_num);
            if names.contains(&device.name) {
                names.insert(&device.name);
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun duplicate device name {}", device.name);
            }
            if device.port > 0 && ports.contains(&device.port) {
                ports.insert(device.port);
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun duplicate port {}", device.port);
            }
        }
        let mut current_port = api_port + 1;
        for device in &mut self.devices {
            if device.port == 0 {
                while ports.contains(&current_port) {
                    current_port += 1;
                }
                device.port = current_port;
                current_port += 1;
            }
        }

        Ok(())
    }
}