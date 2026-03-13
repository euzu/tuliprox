use crate::{
    app::{
        components::{
            userlist::{edit::UserEdit, list::UserlistList, page::UserlistPage},
            Breadcrumbs, Panel, TargetUser,
        },
        context::{api_proxy_users_to_target_users, ConfigContext, UserlistContext},
    },
    i18n::use_translation,
};
use shared::model::TargetUserDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Default)]
pub struct UserlistViewProps {
    #[prop_or_default]
    pub local_mode: bool,
    #[prop_or_default]
    pub users: Option<Vec<TargetUserDto>>,
    #[prop_or_default]
    pub on_users_change: Option<Callback<Vec<TargetUserDto>>>,
}

#[component]
pub fn UserlistView(props: &UserlistViewProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.USERLIST"), translate.t("LABEL.LIST")]));
    let active_page = use_state(|| UserlistPage::List);
    let selected_user = use_state(|| None::<Rc<TargetUser>>);
    let filtered_user = use_state(|| None::<Rc<Vec<Rc<TargetUser>>>>);
    let users = use_state(|| None::<Rc<Vec<Rc<TargetUser>>>>);

    {
        let users_state = users.clone();
        let filtered_user_state = filtered_user.clone();
        use_effect_with(
            (props.local_mode, props.users.clone(), config_ctx.config),
            move |(local_mode, setup_users, api_cfg_opt)| {
                let new_users = if *local_mode {
                    api_proxy_users_to_target_users(setup_users.as_deref().unwrap_or_default())
                } else {
                    api_cfg_opt
                        .as_ref()
                        .and_then(|cfg| cfg.api_proxy.as_ref())
                        .and_then(|api_cfg| api_proxy_users_to_target_users(&api_cfg.user))
                };
                filtered_user_state.set(None);
                users_state.set(new_users);
                || ()
            },
        );
    }

    let on_users_change = props.on_users_change.clone();
    let userlist_context = UserlistContext {
        selected_user: selected_user.clone(),
        filtered_users: filtered_user.clone(),
        users: users.clone(),
        active_page: active_page.clone(),
        local_mode: props.local_mode,
        on_users_change,
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
        let view_visible = active_page.clone();
        let view_visible_dep = *active_page;
        let selected_user = selected_user.clone();
        let selected_user_dep = (*selected_user).clone();
        let translate = translate.clone();
        use_effect_with((view_visible_dep, selected_user_dep), move |_| match *view_visible {
            UserlistPage::List => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.USERS"), translate.t("LABEL.LIST")])),
            UserlistPage::Edit => breadcrumbs.set(Rc::new(vec![
                translate.t("LABEL.USERS"),
                translate.t(if selected_user.is_none() { "LABEL.CREATE" } else { "LABEL.EDIT" }),
            ])),
        });
    };

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
