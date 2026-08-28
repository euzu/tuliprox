use crate::ptt::constants::PTT_CONSTANTS;
use chrono::NaiveDate;

pub fn none(input: &str) -> String { input.to_string() }

pub fn value(val: &str) -> String { val.to_string() }

pub fn boolean(_: &str) -> bool { true }
pub fn uinteger(input: &str) -> Option<u32> { input.parse::<u32>().ok() }

pub fn first_uinteger(input: &str) -> Option<u32> {
    PTT_CONSTANTS.integer.find(input).and_then(|m| m.as_str().parse::<u32>().ok())
}

pub fn lowercase(input: &str) -> String { input.to_lowercase() }

pub fn uppercase(input: &str) -> String { input.to_uppercase() }

pub fn convert_months(date_str: &str) -> String {
    let mut result = date_str.to_string();
    for (re, short) in &PTT_CONSTANTS.months {
        result = re.replace_all(&result, *short).to_string();
    }
    result
}

pub fn date(input: &str, formats: &[&str]) -> Option<String> {
    let sanitized = PTT_CONSTANTS.word.replace_all(input, " ").trim().to_string();
    let sanitized = convert_months(&sanitized);

    for fmt in formats {
        if let Ok(dt) = NaiveDate::parse_from_str(&sanitized, fmt) {
            return Some(dt.format("%Y-%m-%d").to_string());
        }
    }
    None
}

macro_rules! range_func {
    ($name:ident, $t:ty) => {
        pub fn $name(input: &str) -> Option<Vec<$t>> {
            let numbers: Vec<$t> =
                PTT_CONSTANTS.integer.find_iter(input).filter_map(|mat| mat.as_str().parse().ok()).collect();

            if numbers.len() == 2 && numbers[0] < numbers[1] {
                return Some((numbers[0]..=numbers[1]).collect());
            }
            match numbers.len() {
                len if len > 2 => {
                    if numbers.windows(2).all(|w| w[0] + 1 == w[1]) {
                        Some(numbers)
                    } else {
                        None
                    }
                }
                1 => Some(numbers),
                _ => None,
            }
        }
    };
}

range_func!(range_i32, i32);
range_func!(range_u32, u32);
pub fn transform_resolution(input: &str) -> String {
    let lower = input.to_lowercase();
    if lower.contains("2160") || lower.contains("4k") {
        return "2160p".to_string();
    }
    if lower.contains("1440") || lower.contains("2k") {
        return "1440p".to_string();
    }
    if lower.contains("1080") {
        return "1080p".to_string();
    }
    if lower.contains("720") {
        return "720p".to_string();
    }
    if lower.contains("480") {
        return "480p".to_string();
    }
    if lower.contains("360") {
        return "360p".to_string();
    }
    if lower.contains("240") {
        return "240p".to_string();
    }
    input.to_string()
}
