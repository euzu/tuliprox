use crate::{
    app::{TableColumnsPanel, TablePanelSpec},
    error::Error,
    hooks::use_service_context,
    services::TableLayoutSection,
};
use futures::future::{AbortHandle, Abortable};
use gloo_events::EventListener;
use shared::model::UserSettingsDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq, Default)]
pub struct UserSettingsState {
    pub generation: u64,
    pub ready: bool,
    pub settings: UserSettingsDto,
    pub error: Option<Error>,
}

pub enum UserSettingsAction {
    Begin(u64),
    Loaded(u64, Result<UserSettingsDto, Error>),
    Section(u64, String, TableLayoutSection),
}
impl Reducible for UserSettingsState {
    type Action = UserSettingsAction;
    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        match action {
            UserSettingsAction::Begin(generation) => Rc::new(Self { generation, ..Self::default() }),
            UserSettingsAction::Loaded(generation, result) if generation == self.generation => match result {
                Ok(settings) if self.ready && self.error.is_none() && self.settings == settings => self,
                Ok(settings) => Rc::new(Self { generation, ready: true, settings, error: None }),
                Err(err) if self.ready && self.error.as_ref() == Some(&err) => self,
                Err(err) => {
                    let mut state = (*self).clone();
                    state.ready = true;
                    state.error = Some(err);
                    Rc::new(state)
                }
            },
            UserSettingsAction::Section(generation, table, section) if generation == self.generation => {
                if self.settings.section_etags.get(&table) == Some(&section.etag)
                    && self.settings.preferences.web_ui.tables.get(&table).map_or_else(
                        || section.layout.column_order.is_empty() && section.layout.column_visibility.is_empty(),
                        |layout| layout == &section.layout,
                    )
                {
                    return self;
                }
                let mut state = (*self).clone();
                state.settings.section_etags.insert(table.clone(), section.etag);
                state.settings.preferences.web_ui.tables.insert(table, section.layout);
                Rc::new(state)
            }
            _ => self,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct UserSettingsContext {
    pub state: UseReducerHandle<UserSettingsState>,
    pub open_panel: Callback<TablePanelSpec>,
    pub retry_load: Callback<()>,
}

#[derive(Properties, PartialEq)]
pub struct UserSettingsProviderProps {
    pub children: Children,
}

#[component]
pub fn UserSettingsProvider(props: &UserSettingsProviderProps) -> Html {
    let services = use_service_context();
    let state = use_reducer_eq(UserSettingsState::default);
    let panel = use_state_eq(|| None::<TablePanelSpec>);
    let load_epoch = use_mut_ref(|| 0_u64);
    {
        let services = services.clone();
        let state = state.clone();
        let panel = panel.clone();
        let load_epoch = load_epoch.clone();
        use_effect_with((), move |()| {
            let (abort, registration) = AbortHandle::new_pair();
            let auth = services.auth.clone();
            yew::platform::spawn_local(async move {
                let _ = Abortable::new(
                    auth.identity_subscribe(&mut |(generation, identity)| {
                        let services = services.clone();
                        let state = state.clone();
                        *load_epoch.borrow_mut() += 1;
                        let epoch = *load_epoch.borrow();
                        let load_epoch = load_epoch.clone();
                        state.dispatch(UserSettingsAction::Begin(generation));
                        panel.set(None);
                        if identity.is_some() {
                            yew::platform::spawn_local(async move {
                                let result = services.user_settings.load().await;
                                if services.auth.session_generation() == generation && *load_epoch.borrow() == epoch {
                                    state.dispatch(UserSettingsAction::Loaded(generation, result));
                                }
                            });
                        }
                        std::future::ready(())
                    }),
                    registration,
                )
                .await;
            });
            move || abort.abort()
        });
    }
    {
        let services = services.clone();
        let state = state.clone();
        let load_epoch = load_epoch.clone();
        use_effect_with(panel.is_some(), move |open| {
            let listener = if *open {
                None
            } else {
                web_sys::window().map(|window| {
                    EventListener::new(&window, "focus", move |_| {
                        if services.auth.is_authenticated() {
                            let services = services.clone();
                            let state = state.clone();
                            let generation = services.auth.session_generation();
                            *load_epoch.borrow_mut() += 1;
                            let epoch = *load_epoch.borrow();
                            let load_epoch = load_epoch.clone();
                            yew::platform::spawn_local(async move {
                                let result = services.user_settings.load().await;
                                if services.auth.session_generation() == generation && *load_epoch.borrow() == epoch {
                                    state.dispatch(UserSettingsAction::Loaded(generation, result));
                                }
                            });
                        }
                    })
                })
            };
            move || drop(listener)
        });
    }
    let open_panel = {
        let panel = panel.setter();
        let load_epoch = load_epoch.clone();
        use_callback(panel, move |spec, panel| {
            *load_epoch.borrow_mut() += 1;
            panel.set(Some(spec));
        })
    };
    let close = { use_callback(panel.setter(), |(), panel| panel.set(None)) };
    let retry_load = {
        let state = state.dispatcher();
        let load_epoch = load_epoch.clone();
        use_callback((services.clone(), state), move |(), (services, state)| {
            let services = services.clone();
            let state = state.clone();
            let generation = services.auth.session_generation();
            *load_epoch.borrow_mut() += 1;
            let epoch = *load_epoch.borrow();
            let load_epoch = load_epoch.clone();
            yew::platform::spawn_local(async move {
                let result = services.user_settings.load().await;
                if services.auth.session_generation() == generation && *load_epoch.borrow() == epoch {
                    state.dispatch(UserSettingsAction::Loaded(generation, result));
                }
            });
        })
    };
    let context = UserSettingsContext { state, open_panel, retry_load };
    html! { <ContextProvider<UserSettingsContext> context={context}>
        {for props.children.iter()}
        if let Some(spec) = panel.as_ref() { <super::DialogProvider><TableColumnsPanel key={spec.table_id.as_str()} spec={spec.clone()} on_close={close}/></super::DialogProvider> }
    </ContextProvider<UserSettingsContext>> }
}

#[hook]
pub fn use_user_settings() -> Option<UserSettingsContext> { use_context::<UserSettingsContext>() }

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::TableLayoutPreferencesDto;

    #[test]
    fn ignored_and_unchanged_settings_keep_the_existing_state() {
        let section = TableLayoutSection { layout: TableLayoutPreferencesDto::default(), etag: "etag".into() };
        let state = Rc::new(UserSettingsState { generation: 2, ready: true, ..Default::default() })
            .reduce(UserSettingsAction::Section(2, "users".into(), section.clone()));
        let unchanged = state.clone().reduce(UserSettingsAction::Section(2, "users".into(), section.clone()));
        assert!(Rc::ptr_eq(&state, &unchanged));
        let ignored = state.clone().reduce(UserSettingsAction::Section(1, "users".into(), section));
        assert!(Rc::ptr_eq(&state, &ignored));
        let loaded = state.clone().reduce(UserSettingsAction::Loaded(2, Ok(state.settings.clone())));
        assert!(Rc::ptr_eq(&state, &loaded));
        let ignored = state.clone().reduce(UserSettingsAction::Loaded(1, Err(Error::Unauthorized)));
        assert!(Rc::ptr_eq(&state, &ignored));
    }

    #[test]
    fn loading_a_default_section_does_not_materialize_absent_preferences() {
        let mut settings = UserSettingsDto::default();
        settings.section_etags.insert("users".into(), "etag".into());
        let state = Rc::new(UserSettingsState { generation: 2, ready: true, settings, error: None });
        let unchanged = state.clone().reduce(UserSettingsAction::Section(
            2,
            "users".into(),
            TableLayoutSection { layout: TableLayoutPreferencesDto::default(), etag: "etag".into() },
        ));
        assert!(Rc::ptr_eq(&state, &unchanged));
        assert!(!unchanged.settings.preferences.web_ui.tables.contains_key("users"));
    }

    #[test]
    fn load_errors_preserve_preferences_and_new_sessions_clear_them() {
        let state = Rc::new(UserSettingsState { generation: 2, ready: true, ..Default::default() }).reduce(
            UserSettingsAction::Section(
                2,
                "users".into(),
                TableLayoutSection {
                    layout: TableLayoutPreferencesDto { column_order: vec!["username".into()], ..Default::default() },
                    etag: "etag".into(),
                },
            ),
        );
        let failed = state.clone().reduce(UserSettingsAction::Loaded(2, Err(Error::Unauthorized)));
        assert!(failed.ready);
        assert_eq!(failed.settings, state.settings);
        assert_eq!(failed.error, Some(Error::Unauthorized));
        let recovered = failed.reduce(UserSettingsAction::Loaded(2, Ok(state.settings.clone())));
        assert_eq!(recovered.error, None);
        assert_eq!(recovered.settings, state.settings);
        let next = state.reduce(UserSettingsAction::Begin(3));
        assert_eq!(next.generation, 3);
        assert!(!next.ready);
        assert_eq!(next.settings, UserSettingsDto::default());
        assert_eq!(next.error, None);
    }
}
