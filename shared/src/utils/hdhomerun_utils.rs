const CHECKSUM_LOOKUP: [u8; 16] = [
    0xA, 0x5, 0xF, 0x6, 0x7, 0xC, 0x1, 0xB, 0x9, 0x2, 0x8, 0xD, 0x4, 0x3, 0xE, 0x0,
];

fn calculate_checksum(device_id_int: u32) -> u8 {
    let mut checksum: u8 = 0;
    checksum ^= CHECKSUM_LOOKUP[((device_id_int >> 28) & 0x0F) as usize];
    checksum ^= ((device_id_int >> 24) & 0x0F) as u8;
    checksum ^= CHECKSUM_LOOKUP[((device_id_int >> 20) & 0x0F) as usize];
    checksum ^= ((device_id_int >> 16) & 0x0F) as u8;
    checksum ^= CHECKSUM_LOOKUP[((device_id_int >> 12) & 0x0F) as usize];
    checksum ^= ((device_id_int >> 8) & 0x0F) as u8;
    checksum ^= CHECKSUM_LOOKUP[((device_id_int >> 4) & 0x0F) as usize];
    checksum
}

pub fn validate_hdhr_device_id(device_id: &str) -> bool {
    if device_id.len() != 8 || !device_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    if let Ok(device_id_int) = u32::from_str_radix(device_id, 16) {
        let checksum = calculate_checksum(device_id_int);
        return (device_id_int & 0x0F) as u8 == checksum;
    }
    false
}

pub fn generate_hdhr_device_id_from_base(base_id: &str) -> String {
    let base_sanitized: String = base_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if base_sanitized.is_empty() {
        return generate_hdhr_device_id();
    }
    // Keep at most 7 hex digits, pad-left with zeros to 7
    let base7 = format!("{:0>7}", &base_sanitized[..base_sanitized.len().min(7)]);
    if let Ok(base7_int) = u32::from_str_radix(&base7, 16) {
        let base_shifted = base7_int << 4; // bits 4-31 for base, bits 0-3 for checksum
        let checksum = calculate_checksum(base_shifted);
        let final_id = base_shifted | u32::from(checksum);
        format!("{:08X}", final_id)
    } else {
        generate_hdhr_device_id()
    }
}

pub fn generate_hdhr_device_id() -> String {
    // 3 fixed + 4 random = 7 hex digits base
    let rnd = (0..4)
        .map(|_| format!("{:X}", fastrand::u8(0..16)))
        .collect::<String>();
    let base7 = format!("105{rnd}");
    generate_hdhr_device_id_from_base(&base7)
}
