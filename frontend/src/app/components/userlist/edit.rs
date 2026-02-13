use crate::app::components::userlist::proxy_user_credentials_form::ProxyUserCredentialsForm;
use crate::app::components::{Card, TextButton, UserlistContext, UserlistPage};
use crate::app::{ConfigContext, PlaylistContext, TargetUser};
use crate::hooks::use_service_context;
use shared::model::ProxyUserCredentialsDto;
use std::rc::Rc;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn UserEdit() -> Html {
    let translate = use_translation();
    let services_ctx = use_service_context();
    let userlist_ctx = use_context::<UserlistContext>().expect("Userlist context not found");
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let targets = use_memo(playlist_ctx.clone(), |playlist_ctx| {
        match playlist_ctx.sources.as_ref() {
            None => vec![],
            Some(sources) => sources.iter().flat_map(|(_, t)| t).cloned().collect(),
        }
    });

    let server = use_memo(config_ctx.clone(), |config_ctx| {
        match config_ctx.config.as_ref() {
            None => vec![],
            Some(app_config) => match app_config.api_proxy.as_ref() {
                None => vec![],
                Some(api_proxy) => api_proxy.server.to_vec(),
            },
        }
    });

    let handle_cancel = {
        let userlist_ctx = userlist_ctx.clone();
        Callback::from(move |_| {
            userlist_ctx.active_page.set(UserlistPage::List);
            userlist_ctx.selected_user.set(None);
        })
    };

    let handle_back = {
        let handle_cancel = handle_cancel.clone();
        Callback::from(move |_| {
            handle_cancel.emit(());
        })
    };

    let handle_user_save = {
        let userlist = userlist_ctx.clone();
        let handleback = handle_back.clone();
        let services = services_ctx.clone();
        let translate = translate.clone();
        Callback::from(
            move |(is_update, target, user): (bool, String, ProxyUserCredentialsDto)| {
                let services = services.clone();
                let handleback = handleback.clone();
                let userlist = userlist.clone();
                let translate = translate.clone();
                spawn_local(async move {
                    let save_result = if is_update {
                        services
                            .user
                            .update_user(target.clone(), user.clone())
                            .await
                    } else {
                        services
                            .user
                            .create_user(target.clone(), user.clone())
                            .await
                    };
                    match save_result {
                        Ok(()) => {
                            let new_user = Rc::new(TargetUser {
                                target: target.clone(),
                                credentials: Rc::new(user.clone()),
                            });
                            let new_user_list = if let Some(user_list) = userlist.users.as_ref() {
                                let mut new_list: Vec<Rc<TargetUser>> = user_list
                                    .iter()
                                    .map(|target_user| {
                                        let mut cloned_target = target_user.as_ref().clone();
                                        if is_update
                                            && cloned_target.credentials.username == user.username
                                        {
                                            cloned_target.credentials = Rc::new(user.clone());
                                            cloned_target.target = target.clone();
                                        }
                                        Rc::new(cloned_target)
                                    })
                                    .collect();

                                if !is_update {
                                    new_list.push(new_user);
                                }
                                new_list
                            } else {
                                vec![new_user]
                            };
                            userlist.users.set(Some(Rc::new(new_user_list)));
                            handleback.emit(String::new());
                            services
                                .toastr
                                .success(translate.t("MESSAGES.SAVE.USER.SUCCESS"));
                        }
                        Err(err) => {
                            services.toastr.error(err.to_string());
                        }
                    }
                });
            },
        )
    };

    html! {
      <div class="tp__userlist-edit tp__list-create">
        <div class="tp__userlist-edit__header tp__list-create__header">
           <h1>{ translate.t( if userlist_ctx.selected_user.is_none() { "LABEL.CREATE" } else { "LABEL.EDIT" } )}</h1>
           <TextButton class="primary" name="userlist"
               icon="Userlist"
               title={ translate.t("LABEL.LIST")}
               onclick={handle_back}></TextButton>
        </div>
        <div class="tp__userlist-edit__body tp__list-create__body">
            <Card>
               <ProxyUserCredentialsForm server={server.clone()} targets={targets.clone()} user={(*userlist_ctx.selected_user).clone()} on_save={handle_user_save} on_cancel={handle_cancel}/>
            </Card>
        </div>
      </div>
    }
}
