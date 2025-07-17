use crate::app::components::{TextButton, UserlistContext, UserlistPage};
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::userlist::user_table::UserTable;

#[function_component]
pub fn UserlistList() -> Html {
    let translate = use_translation();
    let userlist_ctx = use_context::<UserlistContext>().expect("Userlist context not found");

    let handle_create = {
        let userlist_ctx = userlist_ctx.clone();
        Callback::from(move |_| {
            userlist_ctx.active_page.set(UserlistPage::Edit);
        })
    };

    let userlist_body = if let Some(data) = userlist_ctx.users.as_ref() {
        html! {
            <div class="tp__userlist-list__user">
                <UserTable targets={Some(data.clone())} />
            </div>
    }
    } else {
        html! {  }
    };

    html! {
      <div class="tp__userlist-list tp__list-list">
        <div class="tp__userlist-list__header tp__list-list__header">
          <h1>{ translate.t("LABEL.USERS")}</h1>
          <TextButton style="primary" name="new_userlist"
                icon="PersonAdd"
                title={ translate.t("LABEL.NEW_USER")}
                onclick={handle_create}></TextButton>
        </div>
        <div class="tp__userlist-list__body tp__list-list__body">
           { userlist_body }
        </div>
      </div>
    }
}