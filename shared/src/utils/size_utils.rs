use std::str::FromStr;

#[inline]
pub const fn bytes_to_megabytes(bytes: u64) -> u64 {
    bytes / 1_048_576
}

pub fn parse_size_base_2(size_str: &str) -> Result<u64, String> {
    let units = [
        ("TB", 1_099_511_627_776u64),  // Terabytes
        ("TIB", 1_099_511_627_776u64), // Tebibytes (alias, same multiplier)
        ("GB", 1_073_741_824u64),      // Gigabytes
        ("GIB", 1_073_741_824u64),     // Gibibytes (alias)
        ("MB", 1_048_576u64),          // Megabytes
        ("MIB", 1_048_576u64),         // Mebibytes (alias)
        ("KB", 1_024u64),              // Kilobytes
        ("KIB", 1_024u64),             // Kibibytes (alias)
        ("B", 1u64),                   // Bytes
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
    // Conversion factors to kbps as (numerator, denominator) to avoid integer truncation (e.g. KiB/s = 8.192 kbps)
    let units: &[(&str, u64, u64)] = &[
        ("KB/s", 8, 1),         // Kilobytes per second to kbps
        ("MB/s", 8000, 1),      // Megabytes per second to kbps
        ("KiB/s", 8192, 1000),  // Kibibytes per second to kbps
        ("MiB/s", 8 * 1024, 1), // Mebibytes per second to kbps
        ("kbps", 1, 1),         // Kilobits per second (already in kbps)
        ("Kbps", 1, 1),         // Kilobits per second (already in kbps)
        ("mbps", 1000, 1),      // Megabits per second to kbps
        ("Mbps", 1000, 1),      // Megabits per second to kbps
        ("Mibps", 1024, 1),     // Mebibits per second to kbps
    ];

    let speed_str = input.trim();
    if speed_str.is_empty() {
        return Ok(0);
    }
    for (unit, numerator, denominator) in units {
        if let Some(speed_unit) = speed_str.strip_suffix(unit) {
            let number_part = speed_unit.trim();
            let value = u64::from_str(number_part).map_err(|_| format!("Invalid speed: {number_part}"))?;
            return value
                .checked_mul(*numerator)
                .map(|v| (v + denominator / 2) / denominator)
                .ok_or_else(|| format!("Speed too large: {speed_str}"));
        }
    }

    u64::from_str(speed_str)
        .map_err(|_| format!("Invalid speed: {speed_str}, supported units are {}", join_unit_names(units)))
}

fn join_unit_names(units: &[(&str, u64, u64)]) -> String {
    let mut result = String::new();
    for (idx, (unit, _, _)) in units.iter().enumerate() {
        if idx > 0 {
            result.push(',');
        }
        result.push_str(unit);
    }
    result
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
    use crate::utils::{parse_size_base_2, parse_to_kbps};

    #[test]
    fn test_parse_kpbs() {
        assert_eq!(parse_to_kbps("1KB/s").unwrap(), 8);
        assert_eq!(parse_to_kbps("1MB/s").unwrap(), 8000);
        assert_eq!(parse_to_kbps("1KiB/s").unwrap(), 8 * 1024 / 1000);
        assert_eq!(parse_to_kbps("3KiB/s").unwrap(), 25); // 24.576 rounds up, truncation would give 24
        assert_eq!(parse_to_kbps("8KiB/s").unwrap(), 66); // 65.536 rounds up, truncation would give 65
        assert_eq!(parse_to_kbps("1MiB/s").unwrap(), 8 * 1024);
        assert_eq!(parse_to_kbps("1kbps").unwrap(), 1);
        assert_eq!(parse_to_kbps("1mbps").unwrap(), 1000);
        assert_eq!(parse_to_kbps("1Kbps").unwrap(), 1);
        assert_eq!(parse_to_kbps("1Mbps").unwrap(), 1000);
        assert_eq!(parse_to_kbps("1Mibps").unwrap(), 1024);
    }

    #[test]
    fn test_parse_size_base_2_accepts_kib_mib_gib_aliases() {
        assert_eq!(parse_size_base_2("1KiB").unwrap(), 1024);
        assert_eq!(parse_size_base_2("1MiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_base_2("1GiB").unwrap(), 1024_u64.pow(3));
        assert_eq!(parse_size_base_2("1TiB").unwrap(), 1024_u64.pow(4));
        assert_eq!(parse_size_base_2("2gib").unwrap(), 2 * 1024_u64.pow(3)); // case-insensitive
    }
}
