/// Parses various localized numeric inputs.
/// Examples:
/// "1.234,56" -> 1234.56
/// "1,234.56" -> 1234.56
/// "1,000,23" -> 1000.23  (last comma as decimal separator)
/// "1_000,23" -> 1000.23  (underscore as thousands separator)
pub fn parse_localized_float(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Find the last position of ',' or '.' (the decimal separator)
    let last_comma = s.rfind(',');
    let last_dot = s.rfind('.');
    let sep_pos = match (last_comma, last_dot) {
        (Some(c), Some(d)) => Some(c.max(d)),
        (Some(c), None) => Some(c),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    };

    if let Some(pos) = sep_pos {
        // Everything to the left of the last separator: integer part
        let left = s[..pos]
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '-')
            .collect::<String>();

        // Everything to the right of the last separator: fractional part
        let right = s[pos + 1..]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>();

        // If left is empty (e.g., ",5"), assume "0"
        let left = if left.is_empty() {
            "0".to_string()
        } else {
            left
        };

        let combined = if right.is_empty() {
            left
        } else {
            format!("{}.{}", left, right) // '.' for Rust parsing
        };

        combined.parse::<f64>().ok()
    } else {
        // No decimal separator → clean out all grouping chars and parse as integer
        let cleaned = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '-')
            .collect::<String>();
        cleaned.parse::<f64>().ok()
    }
}

/// Formats an `f64` with a fixed number of decimal places (`decimals`).
/// Thousand grouping (`groups`) is optional — if `true`, groups are separated by underscore.
/// Always uses a comma (`,`) as the decimal separator.
pub fn format_float_localized(value: f64, decimals: usize, groups: bool) -> String {
    let s = format!("{:.*}", decimals, value); // "1234.5678"
    let parts: Vec<&str> = s.split('.').collect();
    let int_part = parts[0];
    let frac_part = parts.get(1).unwrap_or(&"");

    let grouped = if groups {
        let rev_chars: Vec<char> = int_part.chars().rev().collect();
        let mut out = String::new();
        for (i, c) in rev_chars.iter().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push('_');
            }
            out.push(*c);
        }
        out.chars().rev().collect::<String>()
    } else {
        int_part.to_string()
    };

    if frac_part.is_empty() {
        grouped
    } else {
        format!("{},{}", grouped, frac_part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_localized_number_basic() {
        // simple integers
        assert_eq!(parse_localized_float("1234"), Some(1234.0));
        assert_eq!(parse_localized_float("0"), Some(0.0));
        assert_eq!(parse_localized_float("-42"), Some(-42.0));
    }

    #[test]
    fn test_parse_localized_number_decimal_variants() {
        // commas as decimal separator
        assert_eq!(parse_localized_float("1234,56"), Some(1234.56));
        assert_eq!(parse_localized_float("0,5"), Some(0.5));
        assert_eq!(parse_localized_float(",5"), Some(0.5));

        // dots as decimal separator
        assert_eq!(parse_localized_float("1234.56"), Some(1234.56));
        assert_eq!(parse_localized_float(".5"), Some(0.5));
        assert_eq!(parse_localized_float("-.5"), Some(-0.5));

        // multiple separators (last is decimal)
        assert_eq!(parse_localized_float("1,000,23"), Some(1000.23));
        assert_eq!(parse_localized_float("1.000.23"), Some(1000.23));
        assert_eq!(parse_localized_float("1.000,23"), Some(1000.23));
        assert_eq!(parse_localized_float("1,000.23"), Some(1000.23));
    }

    #[test]
    fn test_parse_localized_number_grouping_with_underscores() {
        // underscores as thousand separators
        assert_eq!(parse_localized_float("1_000"), Some(1000.0));
        assert_eq!(parse_localized_float("1_000,5"), Some(1000.5));
        assert_eq!(parse_localized_float("12_345,6789"), Some(12345.6789));
        assert_eq!(parse_localized_float("-12_345,6789"), Some(-12345.6789));
    }

    #[test]
    fn test_parse_localized_number_mixed_grouping() {
        // mixed grouping symbols
        assert_eq!(parse_localized_float("1_000.000,25"), Some(1000000.25));
        assert_eq!(parse_localized_float("1.000_000,25"), Some(1000000.25));
        assert_eq!(parse_localized_float("1,000_000.25"), Some(1000000.25));
        assert_eq!(parse_localized_float("  1_000,25  "), Some(1000.25)); // with spaces
    }

    #[test]
    fn test_format_number_localized_basic() {
        // without grouping
        assert_eq!(format_float_localized(1234.56, 2, false), "1234,56");
        assert_eq!(format_float_localized(0.5, 2, false), "0,50");
        assert_eq!(format_float_localized(12.0, 0, false), "12");
    }

    #[test]
    fn test_format_number_localized_grouped() {
        // with grouping and underscores
        assert_eq!(format_float_localized(1234.56, 2, true), "1_234,56");
        assert_eq!(format_float_localized(1234567.89, 2, true), "1_234_567,89");
        assert_eq!(format_float_localized(1000.0, 0, true), "1_000");
        assert_eq!(format_float_localized(0.5, 3, true), "0,500");
    }

    #[test]
    fn test_format_and_parse_roundtrip() {
        // check that formatted values can be parsed back exactly
        let values = [
            0.0, 1.0, 12.34, 1234.56, 1000.0, 1000000.25, -42.75, 0.0001, 1_234.56,
        ];
        for &v in &values {
            let formatted = format_float_localized(v, 4, true);
            let parsed = parse_localized_float(&formatted).unwrap();
            let diff = (parsed - v).abs();
            assert!(
                diff < 1e-10,
                "roundtrip failed for {v}: {formatted} -> {parsed}"
            );
        }
    }
}
