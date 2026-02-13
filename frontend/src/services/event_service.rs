use crate::model::EventMessage;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

type Subscriber = RefCell<HashMap<usize, Box<dyn Fn(EventMessage)>>>;

pub struct EventService {
    subscriber_id: Rc<AtomicUsize>,
    subscribers: Rc<Subscriber>,
    block_config_updated_message: Rc<AtomicBool>,
    block_epoch: Rc<AtomicUsize>,
}

impl Default for EventService {
    fn default() -> Self {
        Self::new()
    }
}

impl EventService {
    pub fn new() -> Self {
        Self {
            subscriber_id: Rc::new(AtomicUsize::new(0)),
            subscribers: Rc::new(RefCell::new(HashMap::new())),
            block_config_updated_message: Rc::new(AtomicBool::new(false)),
            block_epoch: Rc::new(AtomicUsize::new(0)),
        }
    }

    pub fn is_config_change_message_blocked(&self) -> bool {
        self.block_config_updated_message.load(Ordering::Relaxed)
    }

    pub fn set_config_change_message_blocked(&self, value: bool) {
        if value {
            // Re-block and bump epoch to invalidate any pending unblocks.
            self.block_config_updated_message
                .store(true, Ordering::Release);
            self.block_epoch.fetch_add(1, Ordering::AcqRel);
        } else {
            let flag = Rc::clone(&self.block_config_updated_message);
            let epoch_now = self.block_epoch.load(Ordering::Relaxed);
            let epoch = Rc::clone(&self.block_epoch);
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(500).await;
                if epoch.load(Ordering::Relaxed) == epoch_now {
                    flag.store(false, Ordering::Relaxed);
                }
            });
        }
    }

    pub fn subscribe<F: Fn(EventMessage) + 'static>(&self, callback: F) -> usize {
        let sub_id = self.subscriber_id.fetch_add(1, Ordering::AcqRel);
        self.subscribers
            .borrow_mut()
            .insert(sub_id, Box::new(callback));
        sub_id
    }

    pub fn unsubscribe(&self, sub_id: usize) {
        self.subscribers.borrow_mut().remove(&sub_id);
    }

    pub fn broadcast(&self, msg: EventMessage) {
        for (_, cb) in self.subscribers.borrow().iter() {
            cb(msg.clone());
        }
    }
}
