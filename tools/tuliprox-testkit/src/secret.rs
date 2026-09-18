use crate::TestkitError;

pub fn environment(name: &str) -> Result<String, TestkitError> {
    if name.trim().is_empty() {
        return Err(TestkitError::Configuration("secret environment variable name is empty".to_owned()));
    }
    std::env::var(name)
        .map_err(|_| TestkitError::Configuration(format!("required secret environment variable {name} is not set")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_variable_does_not_expose_a_value() {
        let error = environment("TULIPROX_TESTKIT_MISSING_SECRET").unwrap_err().to_string();
        assert!(error.contains("TULIPROX_TESTKIT_MISSING_SECRET"));
    }
}
