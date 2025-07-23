use std::cell::RefCell;
use std::rc::Rc;
use std::collections::BTreeMap;
use gloo_timers::callback::Interval;
use yew::prelude::*;
use shared::model::StatusCheck;
use crate::hooks::use_service_context;
use crate::services::{ WsMessage};
use yew::platform::spawn_local;

#[hook]
pub fn use_server_status(
    status: UseStateHandle<Option<Rc<StatusCheck>>>,
) ->  UseStateHandle<RefCell<Option<Rc<StatusCheck>>>> {
    let services = use_service_context();
    let status_holder = use_state(|| RefCell::new(None::<Rc<StatusCheck>>));

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();
        let status_holder_signal = status_holder.clone();

        use_effect_with((), move |_| {
            let subid = services_ctx.websocket.subscribe(move |msg| {
                match msg {
                    WsMessage::ServerStatus(server_status) => {
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&server_status));
                        status_signal.set(Some(server_status));
                    }
                    WsMessage::ActiveUser(user_count, connections) => {
                        let mut server_status = {
                            if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                                (**old_status).clone()
                            } else {
                                StatusCheck::default()
                            }
                        };
                        server_status.active_users = user_count;
                        server_status.active_user_connections = connections;
                        let new_status = Rc::new(server_status);
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                        status_signal.set(Some(new_status));
                    }
                    WsMessage::ActiveProvider(provider, connections) => {
                        let mut server_status = {
                            if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                                (**old_status).clone()
                            } else {
                                StatusCheck::default()
                            }
                        };
                        if let Some(treemap) = server_status.active_provider_connections.as_mut() {
                            if connections == 0 {
                                treemap.remove(&provider);
                            } else {
                                treemap.insert(provider, connections);
                            }
                        } else if connections > 0 {
                            let mut treemap = BTreeMap::new();
                            treemap.insert(provider, connections);
                            server_status.active_provider_connections = Some(treemap);
                        }
                        let new_status = Rc::new(server_status);
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                        status_signal.set(Some(new_status));
                    },
                    _ => {}
                }
            });

            let fetch_status = {
                let services_clone = services_ctx.clone();
                move || {
                    let services_clone = services_clone.clone();
                    spawn_local(async move { services_clone.websocket.get_server_status().await; });
                }
            };

            fetch_status();
            let interval = Interval::new(60*1000, move || { fetch_status(); });

            let services_clone = services_ctx.clone();
            move || {
                drop(interval);
                services_clone.websocket.unsubscribe(subid);
            }
        });
    }
    status_holder
}
