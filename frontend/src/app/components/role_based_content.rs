use crate::app::components::api_user::ApiUserView;
use crate::app::{switch, AppRoute};
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_router::Switch;

#[function_component]
pub fn RoleBasedContent() -> Html {
    let services = use_service_context();

    if !services.auth.is_authenticated() {
        return html! { "Not authorized" };
    }

    if services.auth.is_admin() {
        html! {  <Switch<AppRoute> render={switch} /> }
    } else if services.auth.is_user() {
        html! {  <ApiUserView /> }
    } else {
        html! { "Not authorized" }
    }
}
