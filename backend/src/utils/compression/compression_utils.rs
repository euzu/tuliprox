use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::io::{Read, Write};

pub const fn is_gzip(bytes: &[u8]) -> bool {
    // Gzip files start with the bytes 0x1F 0x8B
    bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B
}

/// Reports whether the prefix is a valid RFC 1950 zlib header.
pub const fn is_zlib_header(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }

    let compression_method_and_info = bytes[0];
    let flags = bytes[1];
    let header = u16::from_be_bytes([compression_method_and_info, flags]);
    compression_method_and_info & 0x0f == 8 && compression_method_and_info >> 4 <= 7 && header.rem_euclid(31) == 0
}

pub fn compress_string(input: &str) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input.as_bytes())?;
    encoder.finish()
}

pub fn decompress_string(input: &[u8]) -> std::io::Result<String> {
    let mut decoder = GzDecoder::new(input);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed)?;
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::is_zlib_header;

    #[test]
    fn zlib_header_validation_covers_method_window_checksum_and_short_prefixes() {
        for valid in [[0x78, 0x01], [0x78, 0x5e], [0x78, 0x9c], [0x78, 0xda]] {
            assert!(is_zlib_header(&valid), "expected valid zlib header {valid:02x?}");
        }

        for invalid in [
            &[][..],
            &[0x78][..],
            &[0x79, 0x18][..], // unsupported compression method
            &[0x88, 0x1c][..], // CINFO exceeds the RFC 1950 maximum window size
            &[0x78, 0x02][..], // invalid FCHECK remainder
        ] {
            assert!(!is_zlib_header(invalid), "expected invalid zlib header {invalid:02x?}");
        }
    }
}
