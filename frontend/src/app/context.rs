use std::rc::Rc;
use yew::UseStateHandle;
use shared::model::{AppConfigDto, ConfigTargetDto, PlaylistRequest, ProxyUserCredentialsDto, SearchRequest, StatusCheck, SystemInfo, UiPlaylistCategories};
use crate::app::components::{InputRow, PlaylistEditorPage, PlaylistExplorerPage, UserlistPage};

type SingleSource = (Vec<Rc<InputRow>>, Vec<Rc<ConfigTargetDto>>);

#[derive(Clone, PartialEq)]
pub struct PlaylistContext {
    pub sources: Rc<Option<Rc<Vec<SingleSource>>>>,
}

#[derive(Clone, PartialEq)]
pub struct PlaylistEditorContext {
    pub active_page: UseStateHandle<PlaylistEditorPage>,
}

#[derive(Clone, PartialEq)]
pub struct PlaylistExplorerContext {
    pub active_page: UseStateHandle<PlaylistExplorerPage>,
    pub playlist: UseStateHandle<Option<Rc<UiPlaylistCategories>>>,
    pub playlist_request: UseStateHandle<Option<PlaylistRequest>>,
}

#[derive(Clone, PartialEq)]
pub struct TargetUser {
    pub target: String,
    pub credentials: Rc<ProxyUserCredentialsDto>,
}

pub type TargetUserList = Option<Rc<Vec<Rc<TargetUser>>>>;

#[derive(Clone, PartialEq)]
pub struct UserlistContext {
    pub selected_user: UseStateHandle<Option<Rc<TargetUser>>>,
    pub filtered_users: UseStateHandle<TargetUserList>,
    pub users: UseStateHandle<TargetUserList>,
    pub active_page: UseStateHandle<UserlistPage>,
}

impl UserlistContext {
    pub fn get_users(&self) ->  TargetUserList {
        match &*self.filtered_users {
            Some(filtered) => Some(Rc::clone(filtered)),
            None => (*self.users).clone(),
        }
    }

    pub fn filter(&self, search_req: &SearchRequest) {
        match search_req {
            SearchRequest::Clear => {
                self.filtered_users.set(None);
            },
            SearchRequest::Text(text, search_fields) => {
                let text_lc = text.to_lowercase();
                let filter_username = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "username"));
                let filter_server = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "server"));
                let filter_playlist = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "playlist"));
                let filtered = self.users.as_ref().into_iter()
                    .flat_map(|rc_vec| rc_vec.iter())
                    .filter(|&user_rc| {
                        let user = &**user_rc;
                        let mut matched = false;
                        if filter_username {
                            matched = user.credentials.username.contains(&text_lc);
                        }
                        if !matched && filter_server {
                            matched = user.credentials.server.as_ref().is_some_and(|s| s.contains(&text_lc));
                        }
                        if !matched && filter_playlist {
                            matched = user.target.contains(&text_lc);
                        }
                        matched
                    })
                    .cloned()
                    .collect::<Vec<Rc<TargetUser>>>();
                self.filtered_users.set(Some(Rc::new(filtered)));
            }
            SearchRequest::Regexp(text, search_fields) => {
                if let Ok(regex) = shared::model::REGEX_CACHE.get_or_compile(text) {
                    let filter_username = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "username"));
                    let filter_server = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "server"));
                    let filter_playlist = search_fields.as_ref().is_none_or(|f| f.iter().any(|s| s == "playlist"));
                    let filtered = self.users.as_ref().into_iter()
                        .flat_map(|rc_vec| rc_vec.iter())
                        .filter(|&user_rc| {
                            let user = &**user_rc;
                            let mut matched = false;
                            if filter_username {
                                matched = regex.is_match(&user.credentials.username);
                            }
                            if !matched && filter_server {
                                matched = user.credentials.server.as_ref().is_some_and(|s| regex.is_match(s));
                            }
                            if !matched && filter_playlist {
                                matched = regex.is_match(&user.target);
                            }
                            matched
                        })
                        .cloned()
                        .collect::<Vec<Rc<TargetUser>>>();
                    self.filtered_users.set(Some(Rc::new(filtered)));
                } else {
                    self.filtered_users.set(None);
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct ConfigContext {
    pub config: Option<Rc<AppConfigDto>>,
}

#[derive(Clone, PartialEq)]
pub struct StatusContext {
    pub status: Option<Rc<StatusCheck>>,
    pub system_info: Option<Rc<SystemInfo>>,
}
