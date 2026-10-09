use super::{
    inspector::HlsTsPlaintextRemainder, HlsAes128CbcPrefixDecoder, HlsTsProbeError, HlsTsProbeProtection,
    AES_128_BLOCK_BYTES,
};
use aes::{
    cipher::{Block, BlockDecrypt, KeyInit},
    Aes128,
};
use zeroize::{Zeroize, Zeroizing};

/// Implements the existing HLS AES-128 explicit-IV and sequence-derived-IV rules.
pub fn hls_aes128_cbc_iv(
    explicit_iv: Option<&str>,
    media_sequence: u64,
) -> Result<[u8; AES_128_BLOCK_BYTES], HlsTsProbeError> {
    let mut iv = [0_u8; AES_128_BLOCK_BYTES];
    let Some(explicit_iv) = explicit_iv else {
        iv[AES_128_BLOCK_BYTES - std::mem::size_of::<u64>()..].copy_from_slice(&media_sequence.to_be_bytes());
        return Ok(iv);
    };
    let hex = explicit_iv.strip_prefix("0x").ok_or(HlsTsProbeError::InvalidIv)?;
    if hex.is_empty() || hex.len() > AES_128_BLOCK_BYTES * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HlsTsProbeError::InvalidIv);
    }
    let mut output_index = AES_128_BLOCK_BYTES;
    let mut input_index = hex.len();
    while input_index > 0 {
        let low = hex_digit(hex.as_bytes()[input_index - 1]).ok_or(HlsTsProbeError::InvalidIv)?;
        input_index = input_index.saturating_sub(1);
        let high = if input_index > 0 {
            let value = hex_digit(hex.as_bytes()[input_index - 1]).ok_or(HlsTsProbeError::InvalidIv)?;
            input_index = input_index.saturating_sub(1);
            value
        } else {
            0
        };
        output_index = output_index.checked_sub(1).ok_or(HlsTsProbeError::InvalidIv)?;
        iv[output_index] = (high << 4) | low;
    }
    Ok(iv)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl HlsAes128CbcPrefixDecoder {
    pub(super) fn new(key: &[u8], iv: [u8; AES_128_BLOCK_BYTES]) -> Result<Self, HlsTsProbeError> {
        if key.len() != AES_128_BLOCK_BYTES {
            return Err(HlsTsProbeError::KeyUnavailable);
        }
        let cipher = Aes128::new_from_slice(key).map_err(|_| HlsTsProbeError::KeyUnavailable)?;
        Ok(Self { cipher, previous_ciphertext: iv, carry: Zeroizing::new(Vec::with_capacity(AES_128_BLOCK_BYTES)) })
    }

    pub(super) fn push(&mut self, ciphertext: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut plaintext = Zeroizing::new(Vec::with_capacity(
            self.carry.len().saturating_add(ciphertext.len()) / AES_128_BLOCK_BYTES * AES_128_BLOCK_BYTES,
        ));
        let mut input = ciphertext;
        if !self.carry.is_empty() {
            let required = AES_128_BLOCK_BYTES.saturating_sub(self.carry.len());
            let copied = required.min(input.len());
            self.carry.extend_from_slice(&input[..copied]);
            input = &input[copied..];
            if self.carry.len() == AES_128_BLOCK_BYTES {
                let mut block = [0_u8; AES_128_BLOCK_BYTES];
                block.copy_from_slice(&self.carry);
                self.decrypt_block(block, &mut plaintext);
                self.carry.clear();
            }
        }
        let (chunks, remainder) = input.as_chunks::<AES_128_BLOCK_BYTES>();
        for chunk in chunks {
            let block = *chunk;
            self.decrypt_block(block, &mut plaintext);
        }
        self.carry.extend_from_slice(remainder);
        plaintext
    }

    pub(super) fn decrypt_block(&mut self, ciphertext: [u8; AES_128_BLOCK_BYTES], plaintext: &mut Vec<u8>) {
        let mut decrypted = Block::<Aes128>::default();
        decrypted.copy_from_slice(&ciphertext);
        self.cipher.decrypt_block(&mut decrypted);
        plaintext.extend(decrypted.into_iter().zip(self.previous_ciphertext).map(|(byte, previous)| byte ^ previous));
        self.previous_ciphertext = ciphertext;
    }

    pub(super) fn finish(&self) -> Result<(), HlsTsProbeError> {
        self.carry.is_empty().then_some(()).ok_or(HlsTsProbeError::DecryptionFailed)
    }
}

impl Drop for HlsAes128CbcPrefixDecoder {
    fn drop(&mut self) { self.previous_ciphertext.zeroize(); }
}

pub(super) enum HlsTsSourceDecoder {
    Clear,
    Aes128Cbc(Box<HlsAes128CbcPrefixDecoder>),
}

impl HlsTsSourceDecoder {
    pub(super) fn new(protection: HlsTsProbeProtection<'_>) -> Result<Self, HlsTsProbeError> {
        match protection {
            HlsTsProbeProtection::Clear => Ok(Self::Clear),
            HlsTsProbeProtection::Aes128Cbc { key, iv } => {
                HlsAes128CbcPrefixDecoder::new(key, iv).map(Box::new).map(Self::Aes128Cbc)
            }
        }
    }

    pub(super) fn with_plaintext<T>(&mut self, bytes: &[u8], consume: impl FnOnce(&[u8]) -> T) -> T {
        match self {
            Self::Clear => consume(bytes),
            Self::Aes128Cbc(decoder) => {
                let plaintext = decoder.push(bytes);
                consume(&plaintext)
            }
        }
    }

    pub(super) const fn plaintext_remainder(&self) -> HlsTsPlaintextRemainder {
        match self {
            Self::Clear => HlsTsPlaintextRemainder::ExactPackets,
            Self::Aes128Cbc(_) => HlsTsPlaintextRemainder::Aes128Pkcs7,
        }
    }

    pub(super) fn finish(&self) -> Result<(), HlsTsProbeError> {
        match self {
            Self::Clear => Ok(()),
            Self::Aes128Cbc(decoder) => decoder.finish(),
        }
    }
}
