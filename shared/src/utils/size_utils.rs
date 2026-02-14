use std::str::FromStr;

#[inline]
pub const fn bytes_to_megabytes(bytes: u64) -> u64 { bytes / 1_048_576 }

pub fn parse_size_base_2(size_str: &str) -> Result<u64, String> {
    let units = [
        ("KB", 1_024u64),             // Kilobytes
        ("MB", 1_048_576u64),         // Megabytes
        ("GB", 1_073_741_824u64),     // Gigabytes
        ("TB", 1_099_511_628_000u64), // Terabytes
        ("B", 1u64),                  // Bytes
    ];

    let size_str = size_str.trim().to_uppercase();

    for (unit, multiplier) in &units {
        if size_str.ends_with(unit) {
            let number_part = size_str[..size_str.len() - unit.len()].trim();
            let value = u64::from_str(number_part).map_err(|_| format!("Invalid size: {number_part}"))?;
            return value.checked_mul(*multiplier).ok_or_else(|| format!("Size too large: {size_str}"));
        }
    }

    u64::from_str(&size_str).map_err(|_| format!("Invalid size: {size_str}"))
}

pub fn human_readable_byte_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = bytes as f64;
    let mut unit = units[0];

    for next_unit in units.iter().skip(1) {
        if size < 1024.0 {
            break;
        }
        size /= 1024.0;
        unit = next_unit;
    }

    format!("{size:.2} {unit}")
}

pub fn parse_to_kbps(input: &str) -> Result<u64, String> {
    // Define unit conversion factors (in bits per second)
    let units: &[(&str, u64)] = &[
        ("KB/s", 8),                // Kilobytes per second to kbps
        ("MB/s", 8000),             // Megabytes per second to kbps
        ("KiB/s", 8 * 1024 / 1000), // Kibibytes per second to kbps
        ("MiB/s", 8 * 1024),        // Mebibytes per second to kbps
        ("kbps", 1),                // Kilobits per second (already in kbps)
        ("Kbps", 1),                // Kilobits per second (already in kbps)
        ("mbps", 1000),             // Megabits per second to kbps
        ("Mbps", 1000),             // Megabits per second to kbps
        ("Mibps", 1024),            // Mebibits per second to kbps
    ];

    let speed_str = input.trim();
    if speed_str.is_empty() {
        return Ok(0);
    }
    for (unit, multiplier) in units {
        if let Some(speed_unit) = speed_str.strip_suffix(unit) {
            let number_part = speed_unit.trim();
            let value = u64::from_str(number_part).map_err(|_| format!("Invalid speed: {number_part}"))?;
            return value.checked_mul(*multiplier).ok_or_else(|| format!("Speed too large: {speed_str}"));
        }
    }

    u64::from_str(speed_str).map_err(|_| {
        format!(
            "Invalid speed: {speed_str}, supported units are {}",
            units.iter().map(|p| p.0).collect::<Vec<_>>().join(",")
        )
    })
}

pub fn human_readable_kbps(kbps: u64) -> String {
    let units = ["kbps", "Mbps", "Gbps", "Tbps"];
    let mut speed = kbps as f64;
    let mut unit = units[0];

    for next_unit in units.iter().skip(1) {
        if speed < 1000.0 {
            break;
        }
        speed /= 1000.0;
        unit = next_unit;
    }

    format!("{speed:.2} {unit}")
}

#[cfg(test)]
mod tests {
    use crate::utils::parse_to_kbps;

    #[test]
    fn test_parse_kpbs() {
        assert_eq!(parse_to_kbps("1KB/s").unwrap(), 8);
        assert_eq!(parse_to_kbps("1MB/s").unwrap(), 8000);
        assert_eq!(parse_to_kbps("1KiB/s").unwrap(), 8 * 1024 / 1000);
        assert_eq!(parse_to_kbps("1MiB/s").unwrap(), 8 * 1024);
        assert_eq!(parse_to_kbps("1kbps").unwrap(), 1);
        assert_eq!(parse_to_kbps("1mbps").unwrap(), 1000);
        assert_eq!(parse_to_kbps("1Kbps").unwrap(), 1);
        assert_eq!(parse_to_kbps("1Mbps").unwrap(), 1000);
        assert_eq!(parse_to_kbps("1Mibps").unwrap(), 1024);
    }
}
