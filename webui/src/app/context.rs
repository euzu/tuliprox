use std::rc::Rc;
use yew::UseStateHandle;
use shared::model::{AppConfigDto, ConfigTargetDto, ProxyUserCredentialsDto, StatusCheck};
use crate::app::components::{InputRow, PlaylistPage, UserlistPage};

type SingleSource = (Vec<Rc<InputRow>>, Vec<Rc<ConfigTargetDto>>);

#[derive(Clone, PartialEq)]
pub struct PlaylistContext {
    pub sources: Rc<Option<Rc<Vec<SingleSource>>>>,
    pub active_page: UseStateHandle<PlaylistPage>,
}

#[derive(Clone, PartialEq)]
pub struct TargetUser {
    pub target: String,
    pub credentials: Rc<ProxyUserCredentialsDto>,
}

#[derive(Clone, PartialEq)]
pub struct UserlistContext {
    pub selected_user: UseStateHandle<Option<Rc<TargetUser>>>,
    pub users: Rc<Option<Rc<Vec<Rc<TargetUser>>>>>,
    pub active_page: UseStateHandle<UserlistPage>,
}

#[derive(Clone, PartialEq)]
pub struct ConfigContext {
    pub config: Option<Rc<AppConfigDto>>,
}

#[derive(Clone, PartialEq)]
pub struct StatusContext {
    pub status: Option<Rc<StatusCheck>>,
}
