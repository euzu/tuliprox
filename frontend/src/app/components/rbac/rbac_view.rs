use super::{group_management::GroupManagement, user_management::UserManagement};
use crate::{
    app::components::{TabItem, TabSet},
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{permission::Permission, RbacGroupDto};
use wasm_bindgen_futures::spawn_local;
use yew::{prelude::*, suspense::use_future};

#[component]
pub fn RbacView() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let can_read_users = services.auth.has_permission(Permission::UserRead);

    let groups = use_state(|| None::<Vec<RbacGroupDto>>);

    // Fetch groups once for both tabs
    {
        let services = services.clone();
        let groups = groups.clone();
        let _ = use_future(move || async move {
            if can_read_users {
                if let Ok(Some(result)) = services.rbac.list_groups().await {
                    groups.set(Some(result));
                }
            }
        });
    }

    let on_groups_changed = {
        let services = services.clone();
        let groups = groups.clone();
        Callback::from(move |()| {
            if !can_read_users {
                return;
            }
            let services = services.clone();
            let groups = groups.clone();
            spawn_local(async move {
                if let Ok(Some(result)) = services.rbac.list_groups().await {
                    groups.set(Some(result));
                }
            });
        })
    };

    let tabs = {
        let translate = translate.clone();
        let groups_val = (*groups).clone();
        let on_groups_changed = on_groups_changed.clone();
        use_memo((translate.clone(), groups_val, on_groups_changed), move |(translate, groups_val, on_groups_changed)| {
            vec![
                TabItem {
                    id: "users".to_string(),
                    title: translate.t("LABEL.RBAC_USERS"),
                    icon: "UserOutline".to_string(),
                    children: html! { <UserManagement groups={groups_val.clone()} /> },
                    active_class: None,
                    inactive_class: None,
                },
                TabItem {
                    id: "groups".to_string(),
                    title: translate.t("LABEL.GROUPS"),
                    icon: "Group".to_string(),
                    children: html! { <GroupManagement groups={groups_val.clone()} on_groups_changed={on_groups_changed.clone()} /> },
                    active_class: None,
                    inactive_class: None,
                },
            ]
        })
    };

    html! {
        <div class="tp__config-view">
            <div class="tp__config-view__header">
                <h1>{ translate.t("LABEL.RBAC") }</h1>
            </div>
            <div class="tp__config-view__body">
                <TabSet tabs={tabs} class="tp__config-view__tabset"/>
            </div>
        </div>
    }
}
