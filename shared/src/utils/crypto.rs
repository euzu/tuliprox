use crate::error::TuliproxError;
use aes::Aes128;
use base64::{engine::general_purpose, Engine as _};
use ctr::cipher::{KeyIvInit, StreamCipher};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
const AUTH_TOKEN_VERSION: u8 = 1;
const AUTH_TOKEN_IV_LEN: usize = 16;
const AUTH_TOKEN_MAC_LEN: usize = 32;
const WEB_UI_RESOURCE_DOMAIN: &[u8] = b"tuliprox.web-ui.resource.v1";
const HLS_RESOURCE_DOMAIN: &[u8] = b"tuliprox.hls.resource.v1";

fn token_mac_key(secret: &[u8; 16], domain: &[u8]) -> [u8; AUTH_TOKEN_MAC_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tuliprox.authenticated-token.mac.v1");
    hasher.update(&(domain.len() as u64).to_be_bytes());
    hasher.update(domain);
    hasher.update(secret);
    *hasher.finalize().as_bytes()
}

fn token_mac(secret: &[u8; 16], domain: &[u8], data: &[u8]) -> blake3::Hash {
    blake3::keyed_hash(&token_mac_key(secret, domain), data)
}

fn token_iv(secret: &[u8; 16], domain: &[u8], plaintext: &[u8]) -> [u8; AUTH_TOKEN_IV_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tuliprox.authenticated-token.siv.v1");
    hasher.update(&(domain.len() as u64).to_be_bytes());
    hasher.update(domain);
    hasher.update(secret);
    hasher.update(plaintext);
    let mut iv = [0u8; AUTH_TOKEN_IV_LEN];
    iv.copy_from_slice(&hasher.finalize().as_bytes()[..AUTH_TOKEN_IV_LEN]);
    iv
}

fn apply_token_cipher(secret: &[u8; 16], iv: &[u8; AUTH_TOKEN_IV_LEN], data: &mut [u8]) {
    let mut cipher = Aes128Ctr::new(secret.into(), iv.into());
    cipher.apply_keystream(data);
}

fn encode_authenticated_bytes(secret: &[u8; 16], domain: &[u8], plaintext: &[u8]) -> String {
    let iv = token_iv(secret, domain, plaintext);
    let mut out = Vec::with_capacity(1 + AUTH_TOKEN_IV_LEN + plaintext.len() + AUTH_TOKEN_MAC_LEN);
    out.push(AUTH_TOKEN_VERSION);
    out.extend_from_slice(&iv);
    out.extend_from_slice(plaintext);
    apply_token_cipher(secret, &iv, &mut out[1 + AUTH_TOKEN_IV_LEN..]);
    let mac = token_mac(secret, domain, &out);
    out.extend_from_slice(mac.as_bytes());
    general_purpose::URL_SAFE_NO_PAD.encode(out)
}

pub fn obscure_authenticated_bytes(
    secret: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<String, TuliproxError> {
    Ok(encode_authenticated_bytes(secret, domain, plaintext))
}

pub fn deobscure_authenticated_bytes(
    secret: &[u8; 16],
    domain: &[u8],
    encoded: &str,
) -> Result<Vec<u8>, TuliproxError> {
    let data = general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TuliproxError::Crypto("Can't decode authenticated token".to_string()))?;
    if data.len() < 1 + AUTH_TOKEN_IV_LEN + AUTH_TOKEN_MAC_LEN {
        return Err(TuliproxError::Crypto("Authenticated token is too short".to_string()));
    }
    if data[0] != AUTH_TOKEN_VERSION {
        return Err(TuliproxError::Crypto("Unsupported authenticated token version".to_string()));
    }
    let mac_offset = data.len() - AUTH_TOKEN_MAC_LEN;
    let (authenticated_data, token_mac_bytes) = data.split_at(mac_offset);
    let expected_mac = token_mac(secret, domain, authenticated_data);
    let actual_mac = <[u8; AUTH_TOKEN_MAC_LEN]>::try_from(token_mac_bytes)
        .map(blake3::Hash::from_bytes)
        .map_err(|_| TuliproxError::Crypto("Invalid authenticated token MAC".to_string()))?;
    if expected_mac != actual_mac {
        return Err(TuliproxError::Crypto("Authenticated token MAC mismatch".to_string()));
    }
    let iv: [u8; AUTH_TOKEN_IV_LEN] = authenticated_data[1..=AUTH_TOKEN_IV_LEN]
        .try_into()
        .map_err(|_| TuliproxError::Crypto("Invalid authenticated token IV".to_string()))?;
    let mut plaintext = authenticated_data[1 + AUTH_TOKEN_IV_LEN..].to_vec();
    apply_token_cipher(secret, &iv, &mut plaintext);
    Ok(plaintext)
}

pub fn seal_web_ui_resource_url(secret: &[u8; 16], url: &str) -> String {
    encode_authenticated_bytes(secret, WEB_UI_RESOURCE_DOMAIN, url.as_bytes())
}

pub fn open_web_ui_resource_url(secret: &[u8; 16], encoded: &str) -> Result<String, TuliproxError> {
    if encoded.len() > 16_384 {
        return Err(TuliproxError::Crypto("Resource token is too long".to_string()));
    }
    let bytes = deobscure_authenticated_bytes(secret, WEB_UI_RESOURCE_DOMAIN, encoded)?;
    String::from_utf8(bytes).map_err(|_| TuliproxError::Crypto("Invalid resource URL".to_string()))
}

pub fn seal_hls_resource_url(secret: &[u8; 16], url: &str) -> String {
    encode_authenticated_bytes(secret, HLS_RESOURCE_DOMAIN, url.as_bytes())
}

pub fn open_hls_resource_url(secret: &[u8; 16], encoded: &str) -> Result<String, TuliproxError> {
    if encoded.len() > 16_384 {
        return Err(TuliproxError::Crypto("HLS resource token is too long".to_string()));
    }
    let bytes = deobscure_authenticated_bytes(secret, HLS_RESOURCE_DOMAIN, encoded)?;
    String::from_utf8(bytes).map_err(|_| TuliproxError::Crypto("Invalid HLS resource URL".to_string()))
}

pub fn encode_base64_string(input: &[u8]) -> String { general_purpose::URL_SAFE_NO_PAD.encode(input) }

pub fn decode_base64_string(input: &str) -> Vec<u8> {
    general_purpose::URL_SAFE_NO_PAD.decode(input).unwrap_or_else(|_| input.as_bytes().to_vec())
}

pub fn xor_bytes(secret: &[u8], data: &[u8]) -> Vec<u8> {
    if secret.is_empty() {
        return data.to_vec();
    }
    data.iter().enumerate().map(|(i, &b)| b ^ secret[i % secret.len()]).collect()
}

pub fn obfuscate_text(secret: &[u8], text: &str) -> String { encode_base64_string(&xor_bytes(secret, text.as_bytes())) }

pub fn deobfuscate_text(secret: &[u8], text: &str) -> Result<String, String> {
    let data = xor_bytes(secret, &decode_base64_string(text));
    if let Ok(result) = String::from_utf8(data) {
        Ok(result)
    } else {
        Err(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::{
        deobfuscate_text, obfuscate_text, open_hls_resource_url, open_web_ui_resource_url, seal_hls_resource_url,
        seal_web_ui_resource_url,
    };

    #[test]
    fn test_obfuscate() {
        let mut secret = [0u8; 16];
        for x in &mut secret {
            *x = fastrand::u8(..);
        }
        let plain = "hello world";
        let encrypted = obfuscate_text(&secret, plain);
        let decrypted = deobfuscate_text(&secret, &encrypted).unwrap();

        assert_eq!(decrypted, plain);
    }

    #[test]
    fn resource_tokens_reject_xor_and_cross_domain_tokens() {
        let secret = [7u8; 16];
        let url = "http://192.168.1.20/logo.png";
        let web_token = seal_web_ui_resource_url(&secret, url);
        let hls_token = seal_hls_resource_url(&secret, url);

        assert_eq!(open_web_ui_resource_url(&secret, &web_token).ok().as_deref(), Some(url));
        assert_eq!(open_hls_resource_url(&secret, &hls_token).ok().as_deref(), Some(url));
        assert!(!web_token.contains("192.168.1.20"));
        assert!(open_web_ui_resource_url(&secret, &obfuscate_text(&secret, url)).is_err());
        assert!(open_web_ui_resource_url(&secret, &hls_token).is_err());
        assert!(open_hls_resource_url(&secret, &web_token).is_err());
    }

    #[test]
    fn resource_tokens_reject_modified_ciphertext() {
        use base64::{engine::general_purpose, Engine as _};

        let secret = [7u8; 16];
        let token = seal_web_ui_resource_url(&secret, "http://192.168.1.20/logo.png");
        let mut bytes = general_purpose::URL_SAFE_NO_PAD.decode(token).expect("token bytes");
        bytes[20] ^= 1;
        let tampered = general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        assert!(open_web_ui_resource_url(&secret, &tampered).is_err());
    }
}
