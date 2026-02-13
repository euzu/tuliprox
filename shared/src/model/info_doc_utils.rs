use crate::model::xtream_const;
use serde_json::Value;

pub struct InfoDocUtils {}

impl InfoDocUtils {
    pub fn extract_year_from_release_date(release_date: &str) -> Option<u32> {
        // collect only digits
        let digits_only: String = release_date
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(4)
            .collect();

        // do we have 4 digits?
        if digits_only.len() < 4 {
            return None;
        }

        // and parse year
        digits_only.parse::<u32>().ok()
    }

    pub fn make_bdpath_resource_url(
        resource_url: Option<&str>,
        bd_path: &str,
        index: usize,
        field_prefix: &str,
    ) -> String {
        if let Some(url) = resource_url {
            if bd_path.starts_with("http") {
                return format!(
                    "{url}/{field_prefix}{}_{index}",
                    xtream_const::XC_PROP_BACKDROP_PATH
                );
            }
        }
        bd_path.to_string()
    }

    pub fn limited(n: f64) -> String {
        if n < 0.01 {
            "0".to_string()
        } else {
            let s = format!("{:.2}", n);
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

    pub fn make_resource_url(resource_url: Option<&str>, value: &str, field: &str) -> String {
        if let Some(url) = resource_url {
            if value.starts_with("http") {
                return format!("{url}/{field}");
            }
        }
        value.to_string()
    }
}
