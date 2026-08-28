//! Auth, secret-generation and panel-API defaults.

use crate::error::TuliproxError;

default_eq_fns!(
    default_kick_secs, is_default_kick_secs, u64, 90;
    default_token_ttl_mins, is_default_token_ttl_mins, u32, 30;
    default_auth_error_status, is_default_auth_error_status, u16, 403;
);

pub const fn default_panel_api_provision_timeout_secs() -> u64 {
    65
}
pub const fn default_panel_api_provision_probe_interval_secs() -> u64 {
    15
}
pub const fn default_panel_api_provision_cooldown_secs() -> u64 {
    0
}
pub const fn default_panel_api_alias_pool_min() -> u16 {
    1
}
pub const fn default_panel_api_alias_pool_max() -> u16 {
    1
}

fn fill_with_secure_random_bytes(out: &mut [u8]) -> Result<(), TuliproxError> {
    #[cfg(target_arch = "wasm32")]
    {
        for byte in out {
            *byte = fastrand::u8(..);
        }
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    getrandom::fill(out).map_err(|err| TuliproxError::Crypto(format!("failed to generate random bytes: {err}")))
}

pub fn generate_default_access_secret() -> Result<[u8; 32], TuliproxError> {
    let mut out = [0u8; 32];
    fill_with_secure_random_bytes(&mut out)?;
    Ok(out)
}

pub fn generate_default_encrypt_secret() -> Result<[u8; 16], TuliproxError> {
    let mut out = [0u8; 16];
    fill_with_secure_random_bytes(&mut out)?;
    Ok(out)
}

pub fn default_secret() -> String {
    generate_default_encrypt_secret()
        .map(|secret| secret.iter().map(|b| format!("{b:02X}")).collect())
        .unwrap_or_default()
}
