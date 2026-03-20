use crate::{
    app::{components::api_user::ApiUserView, switch, AppRoute},
    hooks::use_service_context,
    i18n::use_translation,
};
use yew::prelude::*;
use yew_router::Switch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleBasedView {
    Unauthorized,
    MainApp,
    ApiUser,
}

fn resolve_view(is_authenticated: bool, is_api_user: bool) -> RoleBasedView {
    if !is_authenticated {
        RoleBasedView::Unauthorized
    } else if is_api_user {
        RoleBasedView::ApiUser
    } else {
        RoleBasedView::MainApp
    }
}

#[component]
pub fn RoleBasedContent() -> Html {
    let services = use_service_context();
    let translate = use_translation();

    match resolve_view(services.auth.is_authenticated(), services.auth.is_api_user()) {
        RoleBasedView::MainApp => html! { <Switch<AppRoute> render={switch} /> },
        RoleBasedView::ApiUser => html! { <ApiUserView /> },
        RoleBasedView::Unauthorized => html! { <div class="tp__unauthorized">{translate.t("UNAUTHORIZED")}</div> },
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_view, RoleBasedView};

    #[test]
    fn unauthenticated_session_is_unauthorized() {
        assert_eq!(resolve_view(false, false), RoleBasedView::Unauthorized);
    }

    #[test]
    fn web_ui_session_uses_main_app() {
        assert_eq!(resolve_view(true, false), RoleBasedView::MainApp);
    }

    #[test]
    fn api_user_session_uses_api_user_view() {
        assert_eq!(resolve_view(true, true), RoleBasedView::ApiUser);
    }
}
