use aes::Aes128;
use base64::{engine::general_purpose, Engine as _};
use ctr::cipher::{KeyIvInit, StreamCipher};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use shared::error::TuliproxError;
use shared::utils::encode_base64_string;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
const AUTH_TOKEN_VERSION: u8 = 1;
const AES_128_CTR_IV_LEN: usize = 16;
const BLAKE3_MAC_LEN: usize = 32;

pub fn encode_base64_hash(text: &str) -> String {
    let hash = blake3::hash(text.as_bytes());
    encode_base64_string(hash.as_bytes())
}

fn apply_aes_128_ctr(secret: &[u8; 16], iv: &[u8], data: &mut [u8]) -> Result<(), TuliproxError> {
    let mut cipher = Aes128Ctr::new_from_slices(secret, iv)
        .map_err(|_| TuliproxError::Crypto("Can't create AES-CTR cipher".to_string()))?;
    cipher.apply_keystream(data);
    Ok(())
}

fn derive_authenticated_token_mac_key(secret: &[u8; 16], domain: &[u8]) -> [u8; BLAKE3_MAC_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tuliprox.authenticated-token.mac.v1");
    hasher.update(&(domain.len() as u64).to_be_bytes());
    hasher.update(domain);
    hasher.update(secret);
    *hasher.finalize().as_bytes()
}

fn authenticated_token_mac(secret: &[u8; 16], domain: &[u8], data: &[u8]) -> blake3::Hash {
    let mac_key = derive_authenticated_token_mac_key(secret, domain);
    blake3::keyed_hash(&mac_key, data)
}

pub fn obscure_authenticated_bytes(
    secret: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<String, TuliproxError> {
    let mut iv = [0u8; AES_128_CTR_IV_LEN];
    if OsRng.try_fill_bytes(&mut iv).is_err() {
        rand::rng().fill_bytes(&mut iv);
    }

    let data_len = 1 + AES_128_CTR_IV_LEN + plaintext.len();
    let mut out = Vec::with_capacity(data_len + BLAKE3_MAC_LEN);
    out.push(AUTH_TOKEN_VERSION);
    out.extend_from_slice(&iv);
    out.extend_from_slice(plaintext);
    apply_aes_128_ctr(secret, &iv, &mut out[1 + AES_128_CTR_IV_LEN..])?;

    let mac = authenticated_token_mac(secret, domain, &out);
    out.extend_from_slice(mac.as_bytes());
    Ok(general_purpose::URL_SAFE_NO_PAD.encode(out))
}

pub fn deobscure_authenticated_bytes(
    secret: &[u8; 16],
    domain: &[u8],
    encoded: &str,
) -> Result<Vec<u8>, TuliproxError> {
    let data = general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TuliproxError::Crypto("Can't decode authenticated token".to_string()))?;

    let min_len = 1 + AES_128_CTR_IV_LEN + BLAKE3_MAC_LEN;
    if data.len() < min_len {
        return Err(TuliproxError::Crypto("Authenticated token is too short".to_string()));
    }
    if data[0] != AUTH_TOKEN_VERSION {
        return Err(TuliproxError::Crypto("Unsupported authenticated token version".to_string()));
    }

    let mac_offset = data.len() - BLAKE3_MAC_LEN;
    let (authenticated_data, token_mac) = data.split_at(mac_offset);
    let expected_mac = authenticated_token_mac(secret, domain, authenticated_data);
    let token_mac = <[u8; BLAKE3_MAC_LEN]>::try_from(token_mac)
        .map(blake3::Hash::from_bytes)
        .map_err(|_| TuliproxError::Crypto("Invalid authenticated token MAC".to_string()))?;
    if expected_mac != token_mac {
        return Err(TuliproxError::Crypto("Authenticated token MAC mismatch".to_string()));
    }

    let iv_end = 1 + AES_128_CTR_IV_LEN;
    let iv = &authenticated_data[1..iv_end];
    let ciphertext = &authenticated_data[iv_end..];
    let mut plaintext = ciphertext.to_vec();
    apply_aes_128_ctr(secret, iv, &mut plaintext)?;
    Ok(plaintext)
}

pub fn obscure_text(secret: &[u8; 16], url: &str) -> Result<String, TuliproxError> {
    let mut iv = [0u8; 16];
    if OsRng.try_fill_bytes(&mut iv).is_err() {
        rand::rng().fill_bytes(&mut iv);
    }

    let mut ciphertext = url.as_bytes().to_vec();
    apply_aes_128_ctr(secret, &iv, &mut ciphertext)?;

    // IV + Ciphertext -> URL-safe Base64. Keep this format stable for existing resource links.
    let mut out = Vec::with_capacity(iv.len() + ciphertext.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ciphertext);
    Ok(general_purpose::URL_SAFE_NO_PAD.encode(out))
}

pub fn deobscure_text(secret: &[u8; 16], encoded: &str) -> Result<String, TuliproxError> {
    let data = general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TuliproxError::Crypto("Can't decode base64".to_string()))?;

    if data.len() < 16 {
        return Err(TuliproxError::Crypto("Token too short to contain IV".to_string()));
    }

    let (iv, ciphertext) = data.split_at(16);
    let mut plaintext = ciphertext.to_vec();
    apply_aes_128_ctr(secret, iv, &mut plaintext)?;

    String::from_utf8(plaintext).map_err(|_| TuliproxError::Crypto("Can't create utf8 string from decrypted".to_string()))
}

#[cfg(test)]
mod tests {
    use crate::utils::crypto_utils::{
        apply_aes_128_ctr, deobscure_authenticated_bytes, deobscure_text, obscure_authenticated_bytes, obscure_text,
    };
    use base64::{engine::general_purpose, Engine as _};
    use rand::Rng;

    #[test]
    fn test_obscure() {
        let secret: [u8; 16] = rand::rng().random();
        let plain = "hello world";
        let encrypted = obscure_text(&secret, plain).unwrap();
        let decrypted = deobscure_text(&secret, &encrypted).unwrap();

        assert_eq!(decrypted, plain);
    }

    #[test]
    fn aes_ctr_format_matches_existing_openssl_tokens() {
        let secret: [u8; 16] = std::array::from_fn(|i| u8::try_from(i).expect("Invalid secret key length:"));
        let iv: [u8; 16] = std::array::from_fn(|i| u8::try_from(i + 16).expect("Invalid IV length:"));
        let plain = "hello world";
        let expected_token = "EBESExQVFhcYGRobHB0eH2-bgxiO9XQB4mKK";

        let mut ciphertext = plain.as_bytes().to_vec();
        apply_aes_128_ctr(&secret, &iv, &mut ciphertext).unwrap();
        let mut token_bytes = iv.to_vec();
        token_bytes.extend_from_slice(&ciphertext);

        assert_eq!(general_purpose::URL_SAFE_NO_PAD.encode(token_bytes), expected_token);
        assert_eq!(deobscure_text(&secret, expected_token).unwrap(), plain);
    }

    #[test]
    fn authenticated_token_roundtrip_preserves_binary_payload() {
        let secret: [u8; 16] = rand::rng().random();
        let payload = b"\x00playlist-item\xff";

        let token = obscure_authenticated_bytes(&secret, b"provider-resolve", payload).unwrap();
        let decoded = deobscure_authenticated_bytes(&secret, b"provider-resolve", &token).unwrap();

        assert_eq!(decoded, payload);
    }

    #[test]
    fn authenticated_token_rejects_modified_ciphertext() {
        let secret: [u8; 16] = rand::rng().random();
        let token = obscure_authenticated_bytes(&secret, b"provider-resolve", b"payload").unwrap();
        let mut token_bytes = general_purpose::URL_SAFE_NO_PAD.decode(token).unwrap();
        assert!(token_bytes.get(20).is_some(), "token unexpectedly short");
        if let Some(byte) = token_bytes.get_mut(20) {
            *byte ^= 0x01;
        }
        let tampered = general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);

        let decoded = deobscure_authenticated_bytes(&secret, b"provider-resolve", &tampered);

        assert!(decoded.is_err());
    }

    #[test]
    fn authenticated_token_rejects_wrong_domain() {
        let secret: [u8; 16] = rand::rng().random();
        let token = obscure_authenticated_bytes(&secret, b"provider-resolve", b"payload").unwrap();

        let decoded = deobscure_authenticated_bytes(&secret, b"other-domain", &token);

        assert!(decoded.is_err());
    }
}
