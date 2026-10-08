use super::{check_dummy_token, get_base_href, request_post, set_token};
use crate::{
    error::{Error, Error::Unauthorized},
    model::WebConfig,
};
use base64::{engine::general_purpose, Engine as _};
use futures_signals::signal::{Mutable, SignalExt};
use log::warn;
use shared::{
    model::{
        permission::{Permission, PermissionSet, PERM_ALL},
        Claims, TokenResponse, UserCredential, ROLE_ADMIN, ROLE_API_USER, TOKEN_NO_AUTH,
    },
    utils::{concat_path, concat_path_leading_slash},
};
use std::{cell::RefCell, future::Future};

fn decode_jwt_payload(token: &str) -> Option<Claims> {
    let payload_enc = token.split('.').nth(1)?;
    let payload_bytes = general_purpose::URL_SAFE_NO_PAD.decode(payload_enc).ok()?;
    serde_json::from_slice::<Claims>(&payload_bytes).ok()
}

fn resolve_auth_path(config: &WebConfig) -> String {
    let auth_url = config.api.auth_url.trim();
    if !auth_url.is_empty() {
        return auth_url.to_string();
    }

    let base_href = get_base_href();
    concat_path_leading_slash(&base_href, "auth")
}

#[derive(Default)]
struct SessionState {
    generation: u64,
    authenticated: bool,
    identity: Option<String>,
}

impl SessionState {
    fn record_identity(&mut self, identity: String) {
        if self.identity.as_ref().is_some_and(|previous| previous != &identity) {
            self.generation = self.generation.wrapping_add(1);
        }
        self.identity = Some(identity);
    }
}

pub struct AuthService {
    auth_path: String,
    username: RefCell<String>,
    roles: RefCell<Vec<String>>,
    permissions: RefCell<PermissionSet>,
    token_exp: RefCell<Option<i64>>,
    session: Mutable<SessionState>,
}

impl AuthService {
    pub fn new(config: &WebConfig) -> Self {
        Self {
            auth_path: resolve_auth_path(config),
            username: RefCell::new(String::new()),
            session: Mutable::new(SessionState::default()),
            roles: RefCell::new(vec![]),
            permissions: RefCell::new(PermissionSet::new()),
            token_exp: RefCell::new(None),
        }
    }

    pub fn get_username(&self) -> String { self.username.borrow().to_string() }
    pub fn is_admin(&self) -> bool { self.roles.borrow().iter().any(|r| r == ROLE_ADMIN) }

    pub fn is_api_user(&self) -> bool { self.roles.borrow().iter().any(|r| r == ROLE_API_USER) }

    pub fn has_permission(&self, permission: Permission) -> bool {
        self.is_admin() || self.permissions.borrow().contains(permission)
    }

    pub fn has_all_permissions(&self, permissions: PermissionSet) -> bool {
        self.is_admin() || self.permissions.borrow().contains_all(&permissions)
    }

    pub fn has_any_permissions(&self, permissions: PermissionSet) -> bool {
        self.is_admin() || self.permissions.borrow().contains_any(&permissions)
    }

    pub fn is_authenticated(&self) -> bool { self.session.lock_ref().authenticated }

    pub fn token_exp_timestamp(&self) -> Option<i64> { *self.token_exp.borrow() }

    pub async fn auth_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(bool) -> U,
    {
        self.session.signal_ref(|session| session.authenticated).for_each(callback).await;
    }

    pub fn session_generation(&self) -> u64 { self.session.lock_ref().generation }

    pub async fn identity_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut((u64, Option<String>)) -> U,
    {
        self.session
            .signal_ref(|session| (session.generation, session.identity.clone()))
            .dedupe_cloned()
            .for_each(callback)
            .await;
    }

    fn reset_auth_state(&self) {
        self.username.borrow_mut().clear();
        self.roles.borrow_mut().clear();
        *self.permissions.borrow_mut() = PermissionSet::new();
        *self.token_exp.borrow_mut() = None;
        let generation = self.session_generation().wrapping_add(1);
        self.session.set(SessionState { generation, ..SessionState::default() });
    }

    pub fn logout(&self) {
        self.reset_auth_state();
        set_token(None);
    }

    fn unauthorized(&self) -> Result<TokenResponse, Error> {
        self.reset_auth_state();
        set_token(None);
        Err(Unauthorized)
    }

    pub async fn authenticate(&self, username: String, password: String) -> Result<TokenResponse, Error> {
        self.logout();
        let generation = self.session_generation();
        let credentials = UserCredential { username, password };
        match request_post::<UserCredential, TokenResponse>(
            &concat_path(&self.auth_path, "token"),
            credentials,
            None,
            None,
        )
        .await
        {
            Ok(_) if generation != self.session_generation() => Err(Unauthorized),
            Err(_) if generation != self.session_generation() => Err(Unauthorized),
            Ok(Some(token)) => {
                self.username.replace(token.username.clone());
                set_token(Some(&token.token));
                self.handle_token(&token.token);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    pub async fn refresh(&self) -> Result<TokenResponse, Error> {
        let generation = self.session_generation();
        check_dummy_token();
        match request_post::<(), TokenResponse>(&concat_path(&self.auth_path, "refresh"), (), None, None).await {
            Ok(_) if generation != self.session_generation() => Err(Unauthorized),
            Err(_) if generation != self.session_generation() => Err(Unauthorized),
            Ok(Some(token)) => {
                self.username.replace(token.username.clone());
                set_token(Some(&token.token));
                self.handle_token(&token.token);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    fn handle_token(&self, token: &str) {
        let mut roles = self.roles.borrow_mut();
        roles.clear();
        let mut permissions = self.permissions.borrow_mut();
        *permissions = PermissionSet::new();
        *self.token_exp.borrow_mut() = None;
        let mut session = self.session.lock_mut();
        session.authenticated = true;

        if token == TOKEN_NO_AUTH {
            roles.push(ROLE_ADMIN.to_string());
            *permissions = PERM_ALL;
            session.record_identity("local:no_auth".into());
            return;
        }

        if let Some(claims) = decode_jwt_payload(token) {
            for role in claims.roles.names() {
                roles.push(role.to_string());
            }
            if let Some(subject) = &claims.subject_id {
                let identity = if subject.is_builtin_admin() {
                    format!("{}:{}", subject, claims.username.to_ascii_lowercase())
                } else {
                    subject.to_string()
                };
                session.record_identity(identity);
            }
            *permissions = claims.permissions;
            *self.token_exp.borrow_mut() = Some(claims.exp);
        } else {
            warn!("no claims");
        }
    }
}

impl Default for AuthService {
    fn default() -> Self { Self::new(&WebConfig::default()) }
}

#[cfg(test)]
mod tests {
    use super::resolve_auth_path;
    use crate::model::{ApiConfig, WebConfig};

    #[test]
    fn resolve_auth_path_prefers_configured_auth_url() {
        let config = WebConfig {
            api: ApiConfig { api_url: "/tuli/api/v1/".to_string(), auth_url: "/tuli/auth".to_string() },
            ..WebConfig::default()
        };

        assert_eq!(resolve_auth_path(&config), "/tuli/auth");
    }

    #[test]
    fn session_subscriptions_report_refresh_without_reloading_identity() {
        use futures::{pin_mut, FutureExt};
        use std::{cell::RefCell, future::ready};

        let service = super::AuthService::new(&WebConfig {
            api: ApiConfig { auth_url: "/auth".into(), ..ApiConfig::default() },
            ..WebConfig::default()
        });
        let auth_events = RefCell::new(Vec::new());
        let identity_events = RefCell::new(Vec::new());
        let mut on_auth = |authenticated| {
            auth_events.borrow_mut().push(authenticated);
            ready(())
        };
        let mut on_identity = |identity| {
            identity_events.borrow_mut().push(identity);
            ready(())
        };
        let auth_subscription = service.auth_subscribe(&mut on_auth);
        let identity_subscription = service.identity_subscribe(&mut on_identity);
        pin_mut!(auth_subscription, identity_subscription);

        assert!(auth_subscription.as_mut().now_or_never().is_none());
        assert!(identity_subscription.as_mut().now_or_never().is_none());

        service.handle_token(shared::model::TOKEN_NO_AUTH);
        assert!(auth_subscription.as_mut().now_or_never().is_none());
        assert!(identity_subscription.as_mut().now_or_never().is_none());

        service.handle_token(shared::model::TOKEN_NO_AUTH);
        assert!(auth_subscription.as_mut().now_or_never().is_none());
        assert!(identity_subscription.as_mut().now_or_never().is_none());

        service.reset_auth_state();
        assert!(auth_subscription.as_mut().now_or_never().is_none());
        assert!(identity_subscription.as_mut().now_or_never().is_none());
        assert!(!service.is_authenticated());

        service.handle_token(shared::model::TOKEN_NO_AUTH);
        assert!(auth_subscription.as_mut().now_or_never().is_none());
        assert!(identity_subscription.as_mut().now_or_never().is_none());

        assert_eq!(*auth_events.borrow(), [false, true, true, false, true]);
        assert_eq!(
            *identity_events.borrow(),
            [(0, None), (0, Some("local:no_auth".into())), (1, None), (1, Some("local:no_auth".into()))]
        );
    }

    #[test]
    fn session_identity_survives_refresh_and_changes_on_logout_and_auth_mode_switch(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use super::{AuthService, SessionState};
        use base64::Engine as _;
        use futures_signals::signal::Mutable;
        use shared::model::{Claims, PermissionSet, RoleSet, UserId, TOKEN_NO_AUTH};
        use std::cell::RefCell;
        let service = AuthService {
            auth_path: String::new(),
            username: RefCell::new(String::new()),
            roles: RefCell::new(Vec::new()),
            permissions: RefCell::new(PermissionSet::new()),
            token_exp: RefCell::new(None),
            session: Mutable::new(SessionState::default()),
        };
        let mut claims = Claims {
            username: "Alice".into(),
            iss: "test".into(),
            iat: 1,
            exp: 100,
            roles: RoleSet::default(),
            permissions: PermissionSet::new(),
            pwd_version: 0,
            subject_id: Some(UserId::web("Alice")),
            permission_schema_version: 1,
        };
        let token = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
        );
        service.handle_token(&token);
        let initial = service.session_generation();
        service.handle_token(&token);
        assert_eq!(service.session_generation(), initial);
        service.handle_token(TOKEN_NO_AUTH);
        assert_eq!(service.session_generation(), initial + 1);
        service.handle_token(TOKEN_NO_AUTH);
        assert_eq!(service.session_generation(), initial + 1);
        service.reset_auth_state();
        assert_eq!(service.session_generation(), initial + 2);
        assert_eq!(service.session.lock_ref().identity, None);
        service.handle_token(&token);
        assert_eq!(service.session_generation(), initial + 2);
        assert_ne!(service.session_generation(), initial);

        service.reset_auth_state();
        claims.subject_id = None;
        let token = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
        );
        service.handle_token(&token);
        assert!(service.is_authenticated());
        assert_eq!(service.session.lock_ref().identity, None);
        Ok(())
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    wasm_bindgen_test_configure!(run_in_browser);
    #[wasm_bindgen(inline_js = r#"
    let authFetch;
    let resolveAuth;
    export function deferAuth() { authFetch = window.fetch; window.fetch = () => new Promise(resolve => {resolveAuth = resolve;}); }
    export function finishAuth(token) { resolveAuth(new Response(JSON.stringify({token,username:'Alice'}),{status:200,headers:{'Content-Type':'application/json'}})); }
    export function restoreAuthFetch() { window.fetch = authFetch; }
    "#)]
    extern "C" {
        #[wasm_bindgen(js_name=deferAuth)]
        fn defer_auth();
        #[wasm_bindgen(js_name=finishAuth)]
        fn finish_auth(token: &str);
        #[wasm_bindgen(js_name=restoreAuthFetch)]
        fn restore();
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            restore();
            set_token(None);
        }
    }
    async fn settle() {
        TimeoutFuture::new(0).await;
        TimeoutFuture::new(0).await;
    }

    #[wasm_bindgen_test(async)]
    async fn logout_discards_delayed_login_and_refresh_while_normal_refresh_keeps_generation() {
        defer_auth();
        let _guard = Guard;
        let auth = Rc::new(AuthService::new(&WebConfig {
            api: crate::model::ApiConfig { api_url: String::new(), auth_url: "/settings-auth-test".into() },
            ..Default::default()
        }));
        let launch_login = |auth: Rc<AuthService>| {
            let (sender, receiver) = futures::channel::oneshot::channel();
            yew::platform::spawn_local(async move {
                let _ = sender.send(auth.authenticate("Alice".into(), "password".into()).await);
            });
            receiver
        };
        let login = launch_login(auth.clone());
        settle().await;
        let generation = auth.session_generation();
        auth.logout();
        finish_auth(TOKEN_NO_AUTH);
        assert!(matches!(login.await, Ok(Err(Unauthorized))));
        assert!(!auth.is_authenticated());
        assert!(crate::services::get_token().is_none());
        assert_ne!(auth.session_generation(), generation);
        let login = launch_login(auth.clone());
        settle().await;
        finish_auth(TOKEN_NO_AUTH);
        assert!(matches!(login.await, Ok(Ok(_))));
        assert!(auth.is_authenticated());
        let generation = auth.session_generation();
        let launch_refresh = |auth: Rc<AuthService>| {
            let (sender, receiver) = futures::channel::oneshot::channel();
            yew::platform::spawn_local(async move {
                let _ = sender.send(auth.refresh().await);
            });
            receiver
        };
        let refresh = launch_refresh(auth.clone());
        settle().await;
        finish_auth(TOKEN_NO_AUTH);
        assert!(matches!(refresh.await, Ok(Ok(_))));
        assert_eq!(auth.session_generation(), generation);
        assert!(auth.is_authenticated());
        let refresh = launch_refresh(auth.clone());
        settle().await;
        auth.logout();
        finish_auth(TOKEN_NO_AUTH);
        assert!(matches!(refresh.await, Ok(Err(Unauthorized))));
        assert!(!auth.is_authenticated());
        assert!(crate::services::get_token().is_none());
    }
}
