use serde_json::Value;

pub struct InfoDocUtils {}

impl InfoDocUtils {
    #[must_use]
    pub fn extract_year_from_release_date(release_date: &str) -> Option<u32> {
        crate::utils::extract_year(release_date)
    }

    pub fn limited(n: f64) -> String {
        if n < 0.01 {
            "0".to_string()
        } else {
            let s = format!("{n:.2}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
    }

    pub fn build_value(value: Option<&str>) -> Value {
        if let Some(text) = value {
            if let Ok(result) = serde_json::from_str(text) {
                return result;
            }
        }
        Value::Array(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_year_from_release_date_cases() {
        assert_eq!(InfoDocUtils::extract_year_from_release_date("2024-05-01"), Some(2024));
        assert_eq!(InfoDocUtils::extract_year_from_release_date("01.05.2024"), Some(2024));
        assert_eq!(InfoDocUtils::extract_year_from_release_date("1600-01-01"), None);
        assert_eq!(InfoDocUtils::extract_year_from_release_date(""), None);
    }
}
