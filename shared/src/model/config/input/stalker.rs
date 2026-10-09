use super::InputType;
use crate::{
    error::TuliproxError,
    model::{StalkerAuthMode, StalkerInputConfigDto},
};

/// Normalize a user-supplied MAC address into the canonical lowercase
/// `xx:xx:xx:xx:xx:xx` form. Accepts colon-, dash- and bare-hex formats
/// (all three are common in portal provisioning exports). Returns `None`
/// when the value is not a valid 6-octet MAC.
fn normalize_mac_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let octets: Vec<String> = if trimmed.contains(':') || trimmed.contains('-') {
        trimmed.split(['-', ':']).map(str::to_string).collect()
    } else if trimmed.len() == 12 {
        trimmed.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_string()).collect()
    } else {
        return None;
    };
    let valid = octets.len() == 6 && octets.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        return None;
    }
    Some(octets.join(":").to_ascii_lowercase())
}

pub(super) fn prepare_stalker_config(
    input_name: &str,
    input_type: &InputType,
    stalker: &mut Option<StalkerInputConfigDto>,
    is_alias: bool,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), TuliproxError> {
    if !input_type.is_stalker() {
        if stalker.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "stalker configuration is only valid for stalker inputs (input: {input_name})"
            )));
        }
        return Ok(());
    }

    // Aliases inherit the parent's stalker block when they define none of
    // their own — never materialize a default here, otherwise the inherited
    // config would be shadowed by an empty one in `as_input`.
    let config = if is_alias {
        match stalker.as_mut() {
            Some(config) => config,
            None => return Ok(()),
        }
    } else {
        stalker.get_or_insert_with(StalkerInputConfigDto::default)
    };

    let mut has_mac = false;
    // Only normalize an existing device block — do not materialize an empty
    // one (it would be re-serialized as a noise `device: {}` block).
    if let Some(device) = config.device.as_mut() {
        // MAC validation — accept colon-, dash- and bare-hex formats and
        // normalize to lowercase `xx:xx:xx:xx:xx:xx`. Empty MAC is allowed
        // at this point (auth mode might be credentials-only).
        if let Some(mac) = device.mac_address.as_ref() {
            let trimmed = mac.trim();
            if !trimmed.is_empty() {
                let Some(normalized) = normalize_mac_address(trimmed) else {
                    return Err(TuliproxError::ConfigInput(format!(
                        "stalker.device.mac_address must be a MAC address in XX:XX:XX:XX:XX:XX, XX-XX-XX-XX-XX-XX or bare-hex format (input: {input_name})"
                    )));
                };
                let first_octet = u8::from_str_radix(&normalized[..2], 16).unwrap_or_default();
                if first_octet & 1 != 0 {
                    return Err(TuliproxError::ConfigInput(format!(
                        "stalker.device.mac_address must be a unicast MAC address (input: {input_name})"
                    )));
                }
                device.mac_address = Some(normalized);
                has_mac = true;
            }
        }

        // Trim locale/timezone if the user supplied them.
        if let Some(timezone) = device.timezone.as_mut() {
            *timezone = timezone.trim().to_string();
            if timezone.is_empty() {
                device.timezone = None;
            }
        }
        if let Some(locale) = device.locale.as_mut() {
            *locale = locale.trim().to_string();
            if locale.is_empty() {
                device.locale = None;
            }
        }
        if let Some(profile) = device.device_profile.as_mut() {
            *profile = profile.trim().to_string();
            if profile.is_empty() {
                device.device_profile = None;
            }
        }
    }

    // Aliases may inherit the parent's identity, so only main inputs are checked.
    if !is_alias && !input_type.is_batch() {
        let has_credentials = username.is_some_and(|value| !value.trim().is_empty())
            && password.is_some_and(|value| !value.trim().is_empty());
        let requirement = match config.auth_mode {
            StalkerAuthMode::Auto if !has_mac && !has_credentials => {
                Some("stalker.device.mac_address or username/password")
            }
            StalkerAuthMode::MacOnly if !has_mac => Some("stalker.device.mac_address for auth_mode mac_only"),
            StalkerAuthMode::CredentialsOnly if !has_credentials => {
                Some("username and password for auth_mode credentials_only")
            }
            StalkerAuthMode::MacPlusCredentials if !has_mac || !has_credentials => {
                Some("stalker.device.mac_address, username and password for auth_mode mac_plus_credentials")
            }
            _ => None,
        };
        if let Some(requirement) = requirement {
            return Err(TuliproxError::ConfigInput(format!("Stalker input '{input_name}' requires {requirement}")));
        }
    }

    Ok(())
}
