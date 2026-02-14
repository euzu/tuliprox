use zeroize::Zeroize;

pub const TOKEN_NO_AUTH: &str = "authorized";

pub const ROLE_ADMIN: &str = "ADMIN";
pub const ROLE_USER: &str = "USER";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    pub username: String,
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserCredential {
    pub username: String,
    pub password: String,
}

impl UserCredential {
    pub fn zeroize(&mut self) { self.password.zeroize(); }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq, Default)]
pub struct TokenResponse {
    pub token: String,
    pub username: String,
}
