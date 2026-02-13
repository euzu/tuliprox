use crate::app::components::userlist::user_table::UserTable;
use crate::app::components::{DropDownOption, Search, TextButton, UserlistContext, UserlistPage};
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn UserlistList() -> Html {
    let translate = use_translation();
    let userlist_ctx = use_context::<UserlistContext>().expect("Userlist context not found");
    let search_fields = use_memo((), |_| {
        vec![
            DropDownOption::new("username", html! { translate.t("LABEL.NAME") }, true),
            DropDownOption::new("playlist", html! { translate.t("LABEL.PLAYLIST") }, false),
            DropDownOption::new("server", html! { translate.t("LABEL.SERVER") }, false),
        ]
    });

    let handle_create = {
        let userlist_ctx = userlist_ctx.clone();
        Callback::from(move |_| {
            userlist_ctx.active_page.set(UserlistPage::Edit);
        })
    };

    let handle_search = {
        let userlist_ctx = userlist_ctx.clone();
        Callback::from(move |search_req| {
            userlist_ctx.filter(&search_req);
        })
    };

    let userlist_body = if let Some(data) = userlist_ctx.get_users().as_ref() {
        html! {
            <div class="tp__userlist-list__user">
                 <UserTable users={if data.is_empty() {None} else {Some(data.clone())}} />
            </div>
        }
    } else {
        html! {}
    };

    html! {
      <div class="tp__userlist-list tp__list-list">
        <div class="tp__userlist-list__header tp__list-list__header">
          <h1>{ translate.t("LABEL.USERS")}</h1>
          <div class="tp__userlist-list__header-toolbar">
              <Search min_length={1} onsearch={handle_search} options={search_fields.clone()}/>
              <TextButton class="primary" name="new_userlist"
                icon="PersonAdd"
                title={ translate.t("LABEL.NEW_USER")}
                onclick={handle_create}></TextButton>
          </div>
        </div>
        <div class="tp__userlist-list__body tp__list-list__body">
           { userlist_body }
        </div>
      </div>
    }
}
