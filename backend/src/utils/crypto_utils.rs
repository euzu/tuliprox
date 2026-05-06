use aes::Aes128;
use base64::{engine::general_purpose, Engine as _};
use ctr::cipher::{KeyIvInit, StreamCipher};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use shared::error::TuliproxError;
use shared::utils::encode_base64_string;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

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
    use crate::utils::crypto_utils::{apply_aes_128_ctr, deobscure_text, obscure_text};
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
        let secret: [u8; 16] = std::array::from_fn(|i| u8::try_from(i).expect("index out of bounds"));
        let iv: [u8; 16] = std::array::from_fn(|i| u8::try_from(i + 16).expect("index out of bounds"));
        let plain = "hello world";
        let expected_token = "EBESExQVFhcYGRobHB0eH2-bgxiO9XQB4mKK";

        let mut ciphertext = plain.as_bytes().to_vec();
        apply_aes_128_ctr(&secret, &iv, &mut ciphertext).unwrap();
        let mut token_bytes = iv.to_vec();
        token_bytes.extend_from_slice(&ciphertext);

        assert_eq!(general_purpose::URL_SAFE_NO_PAD.encode(token_bytes), expected_token);
        assert_eq!(deobscure_text(&secret, expected_token).unwrap(), plain);
    }
}
