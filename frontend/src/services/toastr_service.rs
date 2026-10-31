use gloo_timers::callback::Timeout;
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
};

/// Longest text a plugin may put in a toast, before the plugin name is added.
const PLUGIN_TOAST_MAX_CHARS: usize = 300;
/// A plugin may show a toast every two seconds, with a burst of three.
const PLUGIN_TOAST_BURST: u32 = 3;
const PLUGIN_TOAST_PER_SECOND: f64 = 0.5;

/// A token bucket over a caller-supplied clock in milliseconds, so a limit can
/// be tested without waiting.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBucket {
    capacity: f64,
    per_ms: f64,
    tokens: f64,
    last_ms: f64,
}

impl TokenBucket {
    /// A full bucket holding `burst` tokens and refilling `per_second` of them.
    pub fn new(burst: u32, per_second: f64, now_ms: f64) -> Self {
        let capacity = f64::from(burst);
        Self { capacity, per_ms: per_second / 1000.0, tokens: capacity, last_ms: now_ms }
    }

    /// Takes one token if there is one.
    pub fn try_take(&mut self, now_ms: f64) -> bool {
        let elapsed = (now_ms - self.last_ms).max(0.0);
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * self.per_ms).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The text of a plugin toast: plain text (control characters other than
/// newlines dropped), cut at 300 characters, prefixed with the plugin name.
pub fn plugin_toast_text(plugin_name: &str, message: &str) -> String {
    let text: String = message.chars().filter(|c| !c.is_control() || *c == '\n').take(PLUGIN_TOAST_MAX_CHARS).collect();
    format!("{plugin_name}: {text}")
}

struct ToastTimer {
    timeout: Option<Timeout>,
    remaining_ms: u32,
    started_at: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
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
    fn default() -> Self { Self { close_mode: ToastCloseMode::Auto(3500) } }
}

#[derive(Clone, PartialEq)]
pub struct Toast {
    pub id: u32,
    pub message: String,
    pub toast_type: ToastType,
    pub close_mode: ToastCloseMode,
}

impl Toast {
    pub fn duration_ms(&self) -> Option<u32> {
        match self.close_mode {
            ToastCloseMode::Auto(ms) => Some(ms),
            ToastCloseMode::Manual => None,
        }
    }
}

#[derive(Default, Clone)]
pub struct ToastrState {
    pub toasts: Vec<Toast>,
}

impl ToastrState {
    pub fn new() -> Self { Self::default() }

    pub fn add_toast(&mut self, toast: Toast) { self.toasts.push(toast); }

    pub fn remove_toast(&mut self, id: u32) { self.toasts.retain(|t| t.id != id); }
}

type ToastrSubscriber = Rc<RefCell<Option<Box<dyn Fn(Vec<Toast>)>>>>;

pub struct ToastrService {
    pub counter: AtomicU32,
    pub state: Rc<RefCell<ToastrState>>,
    subscriber: ToastrSubscriber,
    timers: Rc<RefCell<HashMap<u32, ToastTimer>>>,
    /// One bucket per plugin id, shared by all frames of a plugin.
    plugin_buckets: RefCell<HashMap<String, TokenBucket>>,
}

impl Default for ToastrService {
    fn default() -> Self { Self::new() }
}

impl ToastrService {
    pub fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            state: Rc::new(RefCell::new(ToastrState::default())),
            subscriber: Rc::new(RefCell::new(None)),
            timers: Rc::new(RefCell::new(HashMap::new())),
            plugin_buckets: RefCell::new(HashMap::new()),
        }
    }

    pub fn subscribe<F: Fn(Vec<Toast>) + 'static>(&self, callback: F) {
        self.subscriber.borrow_mut().replace(Box::new(callback));
    }

    pub fn show_with_options(&self, msg: impl Into<String>, toast_type: ToastType, options: ToastOptions) {
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
            self.schedule_dismiss(toast_id, duration_ms);
        }
    }

    fn schedule_dismiss(&self, id: u32, ms: u32) {
        let state_ref = self.state.clone();
        let subscriber_ref = self.subscriber.clone();
        let timers_ref = self.timers.clone();
        let started_at = js_sys::Date::now();
        let timeout = Timeout::new(ms, move || {
            timers_ref.borrow_mut().remove(&id);
            let mut state = state_ref.borrow_mut();
            state.remove_toast(id);
            if let Some(subscriber) = subscriber_ref.borrow().as_ref() {
                subscriber(state.toasts.clone());
            }
        });
        self.timers.borrow_mut().insert(id, ToastTimer { timeout: Some(timeout), remaining_ms: ms, started_at });
    }
    // Pause the auto-dismiss countdown and calculate remaining time
    pub fn pause(&self, id: u32) {
        let mut timers = self.timers.borrow_mut();
        if let Some(entry) = timers.get_mut(&id) {
            if entry.timeout.take().is_some() {
                let elapsed = (js_sys::Date::now() - entry.started_at).max(0.0) as u32;
                entry.remaining_ms = entry.remaining_ms.saturating_sub(elapsed);
            }
        }
    }

    // Resume a previously paused auto-dismiss countdown with the remaining time.
    pub fn resume(&self, id: u32) {
        let remaining = {
            let timers = self.timers.borrow();
            match timers.get(&id) {
                Some(entry) if entry.timeout.is_none() => Some(entry.remaining_ms),
                _ => None,
            }
        };
        if let Some(ms) = remaining {
            self.schedule_dismiss(id, ms.max(1));
        }
    }

    pub fn success(&self, msg: impl Into<String>) {
        self.show_with_options(msg, ToastType::Success, ToastOptions { close_mode: ToastCloseMode::Auto(3000) });
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.show_with_options(msg, ToastType::Error, ToastOptions { close_mode: ToastCloseMode::Auto(4000) });
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.show_with_options(msg, ToastType::Info, ToastOptions { close_mode: ToastCloseMode::Auto(3500) });
    }

    pub fn warning(&self, msg: impl Into<String>) {
        self.show_with_options(msg, ToastType::Warning, ToastOptions { close_mode: ToastCloseMode::Auto(3500) });
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

    /// A toast a plugin frame asked for. Rate-limited per plugin; returns
    /// whether it was shown (anything over the limit is dropped).
    pub fn plugin_toast(&self, plugin_id: &str, plugin_name: &str, toast_type: ToastType, message: &str) -> bool {
        let now = js_sys::Date::now();
        let allowed = self
            .plugin_buckets
            .borrow_mut()
            .entry(plugin_id.to_owned())
            .or_insert_with(|| TokenBucket::new(PLUGIN_TOAST_BURST, PLUGIN_TOAST_PER_SECOND, now))
            .try_take(now);
        if allowed {
            let text = plugin_toast_text(plugin_name, message);
            match toast_type {
                ToastType::Success => self.success(text),
                ToastType::Error => self.error(text),
                ToastType::Info => self.info(text),
                ToastType::Warning => self.warning(text),
            }
        }
        allowed
    }

    // manual close
    pub fn dismiss(&self, id: u32) {
        self.timers.borrow_mut().remove(&id);
        let mut state = self.state.borrow_mut();
        state.remove_toast(id);
        if let Some(subscriber) = self.subscriber.borrow().as_ref() {
            subscriber(state.toasts.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_the_burst_then_refills_over_time() {
        let mut bucket = TokenBucket::new(3, 0.5, 0.0);
        assert!(bucket.try_take(0.0));
        assert!(bucket.try_take(0.0));
        assert!(bucket.try_take(0.0));
        assert!(!bucket.try_take(0.0));
        assert!(!bucket.try_take(1_900.0));
        assert!(bucket.try_take(2_100.0));
        assert!(!bucket.try_take(2_100.0));
    }

    #[test]
    fn bucket_never_holds_more_than_its_burst() {
        let mut bucket = TokenBucket::new(2, 10.0, 0.0);
        assert!(bucket.try_take(0.0));
        // A long pause refills to the capacity, not beyond.
        assert!(bucket.try_take(1_000_000.0));
        assert!(bucket.try_take(1_000_000.0));
        assert!(!bucket.try_take(1_000_000.0));
    }

    #[test]
    fn bucket_ignores_a_clock_that_goes_backwards() {
        let mut bucket = TokenBucket::new(1, 1.0, 5_000.0);
        assert!(bucket.try_take(5_000.0));
        assert!(!bucket.try_take(1_000.0));
    }

    #[test]
    fn plugin_toast_text_is_cut_prefixed_and_plain() {
        assert_eq!(plugin_toast_text("Sports", "hello"), "Sports: hello");
        assert_eq!(plugin_toast_text("P", "a\u{7}b\nc"), "P: ab\nc");
        let long = "x".repeat(500);
        assert_eq!(plugin_toast_text("P", &long), format!("P: {}", "x".repeat(300)));
        let wide = "\u{e9}".repeat(400);
        assert_eq!(plugin_toast_text("P", &wide).chars().count(), 3 + 300);
    }
}
