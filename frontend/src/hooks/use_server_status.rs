use crate::hooks::use_service_context;
use crate::model::EventMessage;
use gloo_timers::callback::Interval;
use shared::model::{ActiveUserConnectionChange, StatusCheck, SystemInfo};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use yew::platform::spawn_local;
use yew::prelude::*;

type ServerStatusState = (
    UseStateHandle<RefCell<Option<Rc<StatusCheck>>>>,
    UseStateHandle<RefCell<Option<Rc<SystemInfo>>>>,
);

#[hook]
pub fn use_server_status(
    status: UseStateHandle<Option<Rc<StatusCheck>>>,
    system_info: UseStateHandle<Option<Rc<SystemInfo>>>,
) -> ServerStatusState {
    let services = use_service_context();
    let status_holder = use_state(|| RefCell::new(None::<Rc<StatusCheck>>));
    let system_info_holder = use_state(|| RefCell::new(None::<Rc<SystemInfo>>));

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();
        let status_holder_signal = status_holder.clone();
        let system_info_signal = system_info.clone();
        let system_info_holder_signal = system_info_holder.clone();

        use_effect_with((), move |_| {
            let subid = services_ctx.event.subscribe(move |msg| match msg {
                EventMessage::ServerStatus(server_status) => {
                    *status_holder_signal.borrow_mut() = Some(Rc::clone(&server_status));
                    status_signal.set(Some(server_status));
                }
                EventMessage::ActiveUser(event) => {
                    let mut server_status = {
                        if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                            (**old_status).clone()
                        } else {
                            StatusCheck::default()
                        }
                    };

                    match event {
                        ActiveUserConnectionChange::Updated(stream_info) => {
                            if let Some(pos) = server_status
                                .active_user_streams
                                .iter()
                                .position(|s| s.addr == stream_info.addr)
                            {
                                server_status.active_user_streams[pos] = stream_info;
                            } else {
                                server_status.active_user_streams.push(stream_info);
                            }
                        }
                        ActiveUserConnectionChange::Disconnected(addr) => {
                            server_status
                                .active_user_streams
                                .retain(|stream_info| stream_info.addr != addr);
                        }
                        ActiveUserConnectionChange::Connections(user_count, connections) => {
                            server_status.active_users = user_count;
                            server_status.active_user_connections = connections;
                        }
                    }

                    let new_status = Rc::new(server_status);
                    *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                    status_signal.set(Some(new_status));
                }
                EventMessage::ActiveProvider(provider, connections) => {
                    let mut server_status = {
                        if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                            (**old_status).clone()
                        } else {
                            StatusCheck::default()
                        }
                    };
                    if let Some(treemap) = server_status.active_provider_connections.as_mut() {
                        treemap.insert(provider, connections);
                    } else {
                        let mut treemap = BTreeMap::new();
                        treemap.insert(provider, connections);
                        server_status.active_provider_connections = Some(treemap);
                    }
                    let new_status = Rc::new(server_status);
                    *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                    status_signal.set(Some(new_status));
                }
                EventMessage::SystemInfoUpdate(system_info) => {
                    let info = Rc::new(system_info);
                    *system_info_holder_signal.borrow_mut() = Some(Rc::clone(&info));
                    system_info_signal.set(Some(info));
                }
                _ => {}
            });

            let fetch_status = {
                let services_clone = services_ctx.clone();
                move || {
                    let services_clone = services_clone.clone();
                    spawn_local(async move {
                        services_clone.websocket.get_server_status().await;
                    });
                }
            };

            fetch_status();
            let interval = Interval::new(60 * 1000, move || {
                fetch_status();
            });

            let services_clone = services_ctx.clone();
            move || {
                drop(interval);
                services_clone.event.unsubscribe(subid);
            }
        });
    }
    (status_holder, system_info_holder)
}
