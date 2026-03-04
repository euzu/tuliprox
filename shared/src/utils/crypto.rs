use base64::{engine::general_purpose, Engine as _};

pub fn encode_base64_string(input: &[u8]) -> String { general_purpose::URL_SAFE_NO_PAD.encode(input) }

pub fn decode_base64_string(input: &str) -> Vec<u8> {
    general_purpose::URL_SAFE_NO_PAD.decode(input).unwrap_or_else(|_| input.as_bytes().to_vec())
}

pub fn xor_bytes(secret: &[u8], data: &[u8]) -> Vec<u8> {
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
    use crate::utils::{deobfuscate_text, obfuscate_text};

    #[test]
    fn test_obfuscate() {
        let mut secret = [0u8; 16];
        for x in &mut secret {
            *x = fastrand::u8(..);
        }
        let plain = "hello world";
        let encrypted = obfuscate_text(&secret, plain);
        let decrypted = deobfuscate_text(&secret, &encrypted.unwrap()).unwrap();

        assert_eq!(decrypted, plain);
    }
}
