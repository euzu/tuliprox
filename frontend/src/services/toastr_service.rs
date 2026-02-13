use gloo_timers::callback::Timeout;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, PartialEq, Default)]
pub enum ToastType {
    Success,
    Error,
    #[default]
    Info,
    Warning,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastCloseMode {
    Auto(u32),
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ToastOptions {
    pub close_mode: ToastCloseMode,
}

impl Default for ToastOptions {
    fn default() -> Self {
        Self {
            close_mode: ToastCloseMode::Auto(3500),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Toast {
    pub id: u32,
    pub message: String,
    pub toast_type: ToastType,
    pub close_mode: ToastCloseMode,
}

#[derive(Default, Clone)]
pub struct ToastrState {
    pub toasts: Vec<Toast>,
}

impl ToastrState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_toast(&mut self, toast: Toast) {
        self.toasts.push(toast);
    }

    pub fn remove_toast(&mut self, id: u32) {
        self.toasts.retain(|t| t.id != id);
    }
}

type ToastrSubscriber = Rc<RefCell<Option<Box<dyn Fn(Vec<Toast>)>>>>;

pub struct ToastrService {
    pub counter: AtomicU32,
    pub state: Rc<RefCell<ToastrState>>,
    subscriber: ToastrSubscriber,
}

impl Default for ToastrService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastrService {
    pub fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            state: Rc::new(RefCell::new(ToastrState::default())),
            subscriber: Rc::new(RefCell::new(None)),
        }
    }

    pub fn subscribe<F: Fn(Vec<Toast>) + 'static>(&self, callback: F) {
        self.subscriber.borrow_mut().replace(Box::new(callback));
    }

    pub fn show_with_options(
        &self,
        msg: impl Into<String>,
        toast_type: ToastType,
        options: ToastOptions,
    ) {
        let mut state = self.state.borrow_mut();
        let toast = Toast {
            id: self.counter.fetch_add(1, Ordering::AcqRel),
            message: msg.into(),
            toast_type: toast_type.clone(),
            close_mode: options.close_mode,
        };
        let toast_id = toast.id;
        state.add_toast(toast);

        if let Some(subscriber) = self.subscriber.borrow().as_ref() {
            subscriber(state.toasts.clone());
        }
        drop(state);

        if let ToastCloseMode::Auto(duration_ms) = options.close_mode {
            let state_ref = self.state.clone();
            let subscriber_ref = self.subscriber.clone();
            Timeout::new(duration_ms, move || {
                let mut state = state_ref.borrow_mut();
                state.remove_toast(toast_id);
                if let Some(subscriber) = subscriber_ref.borrow().as_ref() {
                    subscriber(state.toasts.clone());
                }
            })
            .forget();
        }
    }

    pub fn success(&self, msg: impl Into<String>) {
        self.show_with_options(
            msg,
            ToastType::Success,
            ToastOptions {
                close_mode: ToastCloseMode::Auto(3000),
            },
        );
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.show_with_options(
            msg,
            ToastType::Error,
            ToastOptions {
                close_mode: ToastCloseMode::Auto(4000),
            },
        );
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.show_with_options(
            msg,
            ToastType::Info,
            ToastOptions {
                close_mode: ToastCloseMode::Auto(3500),
            },
        );
    }

    pub fn warning(&self, msg: impl Into<String>) {
        self.show_with_options(
            msg,
            ToastType::Warning,
            ToastOptions {
                close_mode: ToastCloseMode::Auto(3500),
            },
        );
    }

    // Show a Success toast with custom options
    pub fn success_with_options(&self, msg: impl Into<String>, options: ToastOptions) {
        self.show_with_options(msg, ToastType::Success, options);
    }

    // Show an Error toast with custom options
    pub fn error_with_options(&self, msg: impl Into<String>, options: ToastOptions) {
        self.show_with_options(msg, ToastType::Error, options);
    }

    // Show an Info toast with custom options
    pub fn info_with_options(&self, msg: impl Into<String>, options: ToastOptions) {
        self.show_with_options(msg, ToastType::Info, options);
    }

    pub fn warning_with_options(&self, msg: impl Into<String>, options: ToastOptions) {
        self.show_with_options(msg, ToastType::Warning, options);
    }

    // manual close
    pub fn dismiss(&self, id: u32) {
        let mut state = self.state.borrow_mut();
        state.remove_toast(id);
        if let Some(subscriber) = self.subscriber.borrow().as_ref() {
            subscriber(state.toasts.clone());
        }
    }
}
