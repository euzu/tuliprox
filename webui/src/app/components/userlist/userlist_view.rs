use crate::app::components::userlist::edit::UserEdit;
use crate::app::components::userlist::list::UserlistList;
use crate::app::components::userlist::page::UserlistPage;
use crate::app::components::{Breadcrumbs, Panel, TargetUser};
use crate::app::context::{ConfigContext, UserlistContext};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn UserlistView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.USERLIST"), translate.t("LABEL.LIST")]));
    let active_page = use_state(|| UserlistPage::List);
    let selected_user= use_state(|| None::<Rc<TargetUser>>);

    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let users = use_memo(config_ctx.config.as_ref().and_then(|c| c.api_proxy.clone()),
     |api_cfg_opt| {
         if let Some(api_cfg) = api_cfg_opt {
             let mut users = Vec::new();
             for target in &api_cfg.user {
                 for creds in &target.credentials {
                     users.push(Rc::new(TargetUser {
                         target: target.target.to_string(),
                         credentials: Rc::new(creds.clone()),
                     }));
                 }
             }
             Some(Rc::new(users))
         } else {
             None
         }
     });
    
    let userlist_context = UserlistContext {
        selected_user: selected_user.clone(),
        users,
        active_page: active_page.clone(),
    };
    
    let handle_breadcrumb_select = {
        let view_visible = active_page.clone();
        let selected_user = selected_user.clone();
        Callback::from(move |(_name, index)| {
            if index == 0 && *view_visible != UserlistPage::List {
                selected_user.set(None);
                view_visible.set(UserlistPage::List);
            }
        })
    };

    {
        let breadcrumbs = breadcrumbs.clone();
        let view_visible_dep = active_page.clone();
        let view_visible = active_page.clone();
        let selected_user_dep = selected_user.clone();
        let selected_user = selected_user.clone();
        let translate = translate.clone();
        use_effect_with((view_visible_dep, selected_user_dep), move |_| {
            match *view_visible {
                UserlistPage::List => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.USERS"), translate.t("LABEL.LIST")])),
                UserlistPage::Edit => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.USERS"), translate.t( if selected_user.is_none() { "LABEL.CREATE" } else {"LABEL.EDIT" })])),
            }
        });
    };

    // let handle_create = {
    //     Callback::from(move |cmd: String| {})
    // };

    html! {
        <ContextProvider<UserlistContext> context={userlist_context}>
            <div class="tp__userlist-view tp__list-view">
                <Breadcrumbs items={&*breadcrumbs} onclick={ handle_breadcrumb_select }/>
                <div class="tp__userlist-view__body tp__list-view__body">
                    <Panel value={UserlistPage::List.to_string()} active={active_page.to_string()}>
                        <UserlistList />
                    </Panel>
                    <Panel value={UserlistPage::Edit.to_string()} active={active_page.to_string()}>
                        <UserEdit />
                    </Panel>
                </div>
            </div>
        </ContextProvider<UserlistContext>>
    }
}