//! Auth, secret-generation and panel-API defaults.

default_eq_fns!(
    default_kick_secs, is_default_kick_secs, u64, 90;
    default_token_ttl_mins, is_default_token_ttl_mins, u32, 30;
    default_auth_error_status, is_default_auth_error_status, u16, 403;
);

pub const fn default_panel_api_provision_timeout_secs() -> u64 { 65 }
pub const fn default_panel_api_provision_probe_interval_secs() -> u64 { 15 }
pub const fn default_panel_api_provision_cooldown_secs() -> u64 { 0 }
pub const fn default_panel_api_alias_pool_min() -> u16 { 1 }
pub const fn default_panel_api_alias_pool_max() -> u16 { 1 }

fn fill_with_secure_random_bytes(out: &mut [u8]) {
    #[cfg(target_arch = "wasm32")]
    {
        for byte in out {
            *byte = fastrand::u8(..);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    if let Err(err) = getrandom::fill(out) {
        panic!("failed to generate secure random bytes: {err}");
    }
}

pub fn generate_default_access_secret() -> [u8; 32] {
    let mut out = [0u8; 32];
    fill_with_secure_random_bytes(&mut out);
    out
}

pub fn generate_default_encrypt_secret() -> [u8; 16] {
    let mut out = [0u8; 16];
    fill_with_secure_random_bytes(&mut out);
    out
}

pub fn default_secret() -> String { generate_default_encrypt_secret().iter().map(|b| format!("{:02X}", b)).collect() }
