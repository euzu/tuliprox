use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{UserlistContext, UserlistPage, TextButton};

#[function_component]
pub fn UserEdit() -> Html {
    let translate = use_translation();
    let userlist_ctx = use_context::<UserlistContext>().expect("Userlist context not found");

    let handle_back = {
        let userlist_ctx = userlist_ctx.clone();
        Callback::from(move |_| {
            userlist_ctx.active_page.set(UserlistPage::List);
        })
    };

    html! {
      <div class="tp__userlist-edit tp__list-create">
        <div class="tp__userlist-edit__header tp__list-create__header">
           <h1>{ translate.t( if userlist_ctx.selected_user.is_none() { "LABEL.CREATE" } else { "LABEL.EDIT" } )}</h1>
           <TextButton style="primary" name="userlist"
               icon="Userlist"
               title={ translate.t("LABEL.LIST")}
               onclick={handle_back}></TextButton>
        </div>
        <div class="tp__userlist-edit__body tp__list-create__body">
        </div>
      </div>
    }
}