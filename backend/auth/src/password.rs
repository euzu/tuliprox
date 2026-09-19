use rand::{distr::Alphanumeric, Rng};
use shared::error::str_to_io_error;
use zeroize::Zeroize;

fn generate_salt(length: usize) -> String {
    let salt: String = rand::rng().sample_iter(&Alphanumeric).take(length).map(char::from).collect();
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

/// Prompt twice and hash. Both plaintexts are wiped before returning.
///
/// `UserCredential::zeroize` already applies this discipline to a password
/// that arrives over HTTP; a password typed at the terminal deserves the same
/// treatment, and used to be left sitting in two `String`s until their
/// allocations happened to be reused.
pub fn generate_password() -> std::io::Result<String> {
    let mut pwd1 = rpassword::prompt_password("password> ")?;
    let mut pwd2 = rpassword::prompt_password("retype password> ")?;
    let result =
        if pwd1 == pwd2 { generate_password_from_input(&pwd1) } else { Err(str_to_io_error("Passwords don't match")) };
    pwd1.zeroize();
    pwd2.zeroize();
    result
}
