use crate::model::EventMessage;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

/// Ordered by subscription id, so subscribers are notified in subscription order.
type Subscribers = RefCell<BTreeMap<usize, Rc<dyn Fn(EventMessage)>>>;

pub struct EventService {
    next_subscriber_id: Cell<usize>,
    subscribers: Subscribers,
    block_config_updated_message: Rc<Cell<bool>>,
    block_epoch: Rc<Cell<usize>>,
}

impl Default for EventService {
    fn default() -> Self { Self::new() }
}

impl EventService {
    pub fn new() -> Self {
        Self {
            next_subscriber_id: Cell::new(0),
            subscribers: RefCell::new(BTreeMap::new()),
            block_config_updated_message: Rc::new(Cell::new(false)),
            block_epoch: Rc::new(Cell::new(0)),
        }
    }

    pub fn is_config_change_message_blocked(&self) -> bool { self.block_config_updated_message.get() }

    pub fn set_config_change_message_blocked(&self, value: bool) {
        if value {
            // Re-block and bump epoch to invalidate any pending unblocks.
            self.block_config_updated_message.set(true);
            self.block_epoch.set(self.block_epoch.get().wrapping_add(1));
        } else {
            let flag = Rc::clone(&self.block_config_updated_message);
            let epoch_now = self.block_epoch.get();
            let epoch = Rc::clone(&self.block_epoch);
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(500).await;
                if epoch.get() == epoch_now {
                    flag.set(false);
                }
            });
        }
    }

    pub fn subscribe<F: Fn(EventMessage) + 'static>(&self, callback: F) -> usize {
        let sub_id = self.next_subscriber_id.get();
        self.next_subscriber_id.set(sub_id.wrapping_add(1));
        self.subscribers.borrow_mut().insert(sub_id, Rc::new(callback));
        sub_id
    }

    pub fn unsubscribe(&self, sub_id: usize) {
        // Release the borrow before dropping the callback; its captures may re-enter the service.
        let removed = self.subscribers.borrow_mut().remove(&sub_id);
        drop(removed);
    }

    pub fn broadcast(&self, msg: EventMessage) {
        // Callbacks can remove subscriptions or broadcast again during logout.
        let subscribers: Vec<_> = self.subscribers.borrow().iter().map(|(&id, cb)| (id, Rc::clone(cb))).collect();
        for (id, cb) in subscribers {
            if self.subscribers.borrow().contains_key(&id) {
                cb(msg.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EventService;
    use crate::model::EventMessage;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn logout_callback_can_unsubscribe_and_broadcast_disconnection() {
        let service = Rc::new(EventService::new());
        let subscription = Rc::new(Cell::new(0));
        let disconnected = Rc::new(Cell::new(false));
        let observed = Rc::clone(&disconnected);
        service.subscribe(move |msg| {
            if msg == EventMessage::WebSocketStatus(false) {
                observed.set(true);
            }
        });
        let events = Rc::clone(&service);
        let id = Rc::clone(&subscription);
        subscription.set(service.subscribe(move |msg| {
            if msg == EventMessage::Unauthorized {
                events.unsubscribe(id.get());
                events.broadcast(EventMessage::WebSocketStatus(false));
            }
        }));

        service.broadcast(EventMessage::Unauthorized);
        assert!(disconnected.get());
        service.broadcast(EventMessage::Unauthorized);
    }

    #[test]
    fn callbacks_can_subscribe_during_broadcast() {
        let service = Rc::new(EventService::new());
        let calls = Rc::new(Cell::new(0));
        let events = Rc::clone(&service);
        let observed = Rc::clone(&calls);
        let id = service.subscribe(move |_| {
            let observed = Rc::clone(&observed);
            events.subscribe(move |_| observed.set(observed.get() + 1));
        });

        service.broadcast(EventMessage::Unauthorized);
        assert_eq!(calls.get(), 0);
        service.unsubscribe(id);
        service.broadcast(EventMessage::Unauthorized);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn removed_subscribers_are_skipped_in_the_current_broadcast() {
        let service = Rc::new(EventService::new());
        let calls = Rc::new(Cell::new(0));
        let ids = Rc::new(Cell::new([0, 0]));
        let subscriptions = std::array::from_fn(|_| {
            let events = Rc::clone(&service);
            let subscriptions = Rc::clone(&ids);
            let calls = Rc::clone(&calls);
            service.subscribe(move |_| {
                calls.set(calls.get() + 1);
                for id in subscriptions.get() {
                    events.unsubscribe(id);
                }
            })
        });
        ids.set(subscriptions);
        service.broadcast(EventMessage::Unauthorized);
        assert_eq!(calls.get(), 1);
    }
}
