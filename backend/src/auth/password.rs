use rand::{Rng};
use rand::distr::Alphanumeric;
use shared::error::str_to_io_error;

fn generate_salt(length: usize) -> String {
    let salt: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect();
    salt
}

pub fn hash(password: &[u8]) -> Option<String> {
    let salt = generate_salt(64);
    if !password.is_empty() {
        let config = argon2::Config::default();
        if let Ok(hash) = argon2::hash_encoded(password, salt.as_bytes(), &config) {
            return Some(hash);
        }
    }
    None
}

pub fn verify_password(hash: &str, password: &[u8]) -> bool {
    if let Ok(valid) = argon2::verify_encoded(hash, password) {
        return valid;
    }
    false
}

pub fn generate_password_from_input(password: &str) -> std::io::Result<String> {
    if password.len() < 8 {
        return Err(str_to_io_error("Password too short min length 8"));
    }
    hash(password.as_bytes()).map_or_else(|| Err(str_to_io_error("Failed to generate hash")), Ok)
}

pub fn generate_password() -> std::io::Result<String> {
    match rpassword::prompt_password("password> ") {
        Ok(pwd1) => {
            match rpassword::prompt_password("retype password> ") {
                Ok(pwd2) => {
                    if pwd1.eq(&pwd2) {
                        generate_password_from_input(&pwd1)
                    } else {
                        Err(str_to_io_error("Passwords don't match"))
                    }
                }
                Err(err) => Err(err)
            }
        },
        Err(err) => Err(err)
    }
}
