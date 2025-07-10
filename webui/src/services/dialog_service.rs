use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use yew::{Callback, Html};
use crate::model::{DialogAction, DialogActions, DialogResult};

#[derive(Clone)]
pub struct DialogFuture {
    result: Rc<RefCell<Option<DialogResult>>>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl DialogFuture {
    fn new() -> (Self, impl Fn(DialogResult) + 'static) {
        let result = Rc::new(RefCell::new(None));
        let waker = Rc::new(RefCell::new(None));

        let result_clone = result.clone();
        let waker_clone = waker.clone();

        let resolve = move |value: DialogResult| {
            *result_clone.borrow_mut() = Some(value);
            if let Some(the_waker) = waker_clone.borrow_mut().take() {
                Waker::wake(the_waker);
            }
        };

        (
            DialogFuture { result, waker },
            resolve,
        )
    }
}

impl Future for DialogFuture {
    type Output = DialogResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = self.result.borrow_mut().take() {
            Poll::Ready(value)
        } else {
            *self.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[derive(Clone)]
pub struct ConfirmRequest {
    pub title: String,
    pub ok_caption: String,
    pub cancel_caption: String,
    pub resolve: Rc<RefCell<Option<Box<dyn Fn(DialogResult)>>>>,
}

#[derive(Clone)]
pub struct ContentRequest {
    pub content: Html,
    pub actions: DialogActions,
    pub resolve: Rc<RefCell<Option<Box<dyn Fn(DialogResult)>>>>,
}

#[derive(Clone)]
pub enum DialogRequest {
    Confirm(ConfirmRequest),
    Content(ContentRequest),
}

#[derive(Default, Clone, PartialEq)]
pub struct DialogService {
    inner: Rc<RefCell<Option<Callback<DialogRequest>>>>,
}

impl DialogService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, cb: Callback<DialogRequest>) {
        *self.inner.borrow_mut() = Some(cb);
    }

    pub fn confirm(&self, title: &str) -> DialogFuture {
        let (future, resolver) = DialogFuture::new();
        let request = ConfirmRequest {
            title: title.to_string(),
            ok_caption: "LABEL.OK".into(),
            cancel_caption: "LABEL.CANCEL".into(),
            resolve: Rc::new(RefCell::new(Some(Box::new(resolver)))),
        };

        if let Some(cb) = &*self.inner.borrow() {
            cb.emit(DialogRequest::Confirm(request));
        }

        future
    }

    pub fn content(&self, content: Html, actions: Option<DialogActions>) -> DialogFuture {
        let (future, resolver) = DialogFuture::new();
        let request = ContentRequest {
            content,
            actions: actions.unwrap_or_else(||  DialogActions {
                left: None,
                right: vec![ DialogAction::new_focused("close", "LABEL.CLOSE",  DialogResult::Cancel, Some("Close".to_string()), None) ],
            }),
            resolve: Rc::new(RefCell::new(Some(Box::new(resolver)))),
        };

        if let Some(cb) = &*self.inner.borrow() {
            cb.emit(DialogRequest::Content(request));
        }

        future
    }
}
