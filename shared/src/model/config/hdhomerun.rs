use crate::info_err_res;
use crate::error::{TuliproxError};
use crate::utils::{is_false, is_true, default_as_true, default_device_type, default_device_udn,
                   default_firmware_name, default_firmware_version, default_friendly_name,
                   default_manufacturer, default_model_name, generate_hdhr_device_id,
                   generate_hdhr_device_id_from_base, hash_string, is_default_device_type,
                   is_default_device_udn, is_default_firmware_name, is_default_firmware_version,
                   is_default_friendly_name, is_default_manufacturer, is_default_model_name,
                   validate_hdhr_device_id};
use log::warn;
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunDeviceConfigDto {
    #[serde(default = "default_friendly_name", skip_serializing_if = "is_default_friendly_name")]
    pub friendly_name: String,
    #[serde(default = "default_manufacturer", skip_serializing_if = "is_default_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_model_name", skip_serializing_if = "is_default_model_name")]
    pub model_name: String,
    #[serde(default = "default_model_name", skip_serializing_if = "is_default_model_name")]
    pub model_number: String,
    #[serde(default = "default_firmware_name", skip_serializing_if = "is_default_firmware_name")]
    pub firmware_name: String,
    #[serde(default = "default_firmware_version", skip_serializing_if = "is_default_firmware_version")]
    pub firmware_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(default = "default_device_type", skip_serializing_if = "is_default_device_type")]
    pub device_type: String,
    #[serde(default = "default_device_udn", skip_serializing_if = "is_default_device_udn")]
    pub device_udn: String,
    pub name: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub tuner_count: u8,
}

impl Default for HdHomeRunDeviceConfigDto {
    fn default() -> Self {
        Self {
            friendly_name: default_friendly_name(),
            manufacturer: default_manufacturer(),
            model_name: default_model_name(),
            model_number: default_model_name(),
            firmware_name: default_firmware_name(),
            firmware_version: default_firmware_version(),
            device_id: String::new(),
            device_type: default_device_type(),
            device_udn: default_device_udn(),
            name: String::new(),
            port: 0,
            tuner_count: 0,
        }
    }
}

impl HdHomeRunDeviceConfigDto {
    pub fn prepare(&mut self, device_num: u8, include_computed: bool) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = format!("device{device_num}");
            if include_computed {
                warn!("Device name empty, assigned new name: {}", self.name);
            }
        }

        if self.tuner_count == 0 {
            self.tuner_count = 1;
        }

        if device_num > 0 && self.friendly_name == default_friendly_name() {
            self.friendly_name = format!("{} {}", self.friendly_name, device_num);
        }

        // --- UDN Logic ---
        // If UDN is the default value or empty, generate a stable UUID from the device name.
        if self.device_udn == default_device_udn() || self.device_udn.is_empty() {
            let hash = hash_string(&self.name);
            // Format the hash into a valid UUID string
            self.device_udn = hash.to_valid_uuid();
            if include_computed {
                warn!("HDHomeRun device '{}' is missing a unique device_udn. A new one has been generated: {}", self.name, self.device_udn);
            }
        } else {
            // Ensure only the UUID part is stored.
            if let Some(uuid_part) = self.device_udn.strip_prefix("uuid:") {
                self.device_udn = uuid_part.split("::").next().unwrap_or(uuid_part).to_string();
            }
        }

        // --- Device ID Logic ---
        if self.device_id.is_empty() {
            self.device_id = generate_hdhr_device_id();
            if include_computed {
                warn!("HDHomeRun device '{}' is missing a device_id. A new one has been generated: {}", self.name, self.device_id);
            }
        } else if !validate_hdhr_device_id(&self.device_id) {
            let old_id = self.device_id.clone();
            self.device_id = generate_hdhr_device_id_from_base(&self.device_id);
            if include_computed {
                warn!("HDHomeRun device '{}' has an invalid device_id '{}'. A valid one has been generated: {}", self.name, old_id, self.device_id);
            }
        }
        Ok(())
    }
}


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub auth: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub ssdp_discovery: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub proprietary_discovery: bool,
    pub devices: Vec<HdHomeRunDeviceConfigDto>,
}

impl HdHomeRunConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && !self.auth && self.devices.is_empty() && !self.ssdp_discovery && !self.proprietary_discovery
    }

    pub fn clean(&mut self) {
        // This method can be left empty if no cleanup is required.
        // It's only included to satisfy the frontend compiler.
    }

    pub fn prepare(&mut self, api_port: u16, include_computed: bool) -> Result<(), TuliproxError> {
        let mut names = HashSet::new();
        let mut ports = HashSet::new();
        let mut device_ids = HashSet::new();
        ports.insert(api_port);
        for (device_num, device) in (0_u8..).zip(self.devices.iter_mut()) {
            device.prepare(device_num, include_computed)?;
            if !names.insert(device.name.clone()) {
                return info_err_res!("HdHomeRun duplicate device name {}", device.name);
            }
            if device.port > 0 && !ports.insert(device.port) {
                return info_err_res!("HdHomeRun duplicate port {}", device.port);
            }
            if !device_ids.insert(device.device_id.clone()) {
                return info_err_res!("HdHomeRun duplicate device_id {}", device.device_id);
            }
        }
        let mut current_port = api_port.saturating_add(1);
        for device in &mut self.devices {
            if device.port == 0 {
                while ports.contains(&current_port) || current_port == 0 {
                    current_port = current_port.wrapping_add(1);
                    if current_port == api_port { // full cycle guard
                        return info_err_res!("No free port available for HdHomeRun devices");
                    }
                }

                device.port = current_port;
                ports.insert(current_port);
                current_port += 1;
            }
        }
        Ok(())
    }
}