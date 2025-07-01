pub trait Capitalize {
    fn capitalize(&self) -> String;
}

// Implement the Capitalize trait for &str
impl<T: AsRef<str>> Capitalize for T {
    fn capitalize(&self) -> String {
        let s = self.as_ref();
        let mut chars = s.chars();
        let first = chars
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default();
        let rest = chars.as_str().to_lowercase();
        first + &rest
    }
}

pub fn get_trimmed_string(value: &Option<String>) -> Option<String> {
    if let Some(v) = value {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub fn generate_random_string(length: usize) -> String {
    let charset = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let range = ..charset.len();
    let random_string: String = (0..length)
        .map(|_| {
            let idx = fastrand::usize(range);
            charset[idx] as char
        })
        .collect();

    random_string
}

// compare 2 small vecs without HashSet
pub fn small_vecs_equal_unordered<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    for item in a {
        if !b.iter().any(|x| x == item) {
            return false;
        }
    }
    true
}

pub fn get_non_empty_str<'a>(first: &'a str, second: &'a str, third: &'a str) -> &'a str {
    if !first.is_empty() {
        first
    } else if !second.is_empty() {
        second
    } else {
        third
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use crate::utils::Capitalize;
    use super::generate_random_string;

    #[test]
    fn test_generate_random_string() {
        let mut strings = HashSet::new();
        for _i in 0..100 {
            strings.insert(generate_random_string(5));
        }
        assert_eq!(strings.len(), 100);
    }

    #[test]
    fn test_capitalize() {
        assert_eq!("hELLO".capitalize(), "Hello");
    }

}