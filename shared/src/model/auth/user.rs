use zeroize::Zeroize;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserCredential {
    pub username: String,
    pub password: String,
}

impl UserCredential {
    pub fn zeroize(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq, Default)]
pub struct TokenResponse {
    pub token: String,
    pub username: String,
}
