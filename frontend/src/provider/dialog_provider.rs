use crate::{
    app::{ConfirmDialog, ContentDialog},
    model::DialogResult,
    services::{DialogRequest, DialogService},
};
use std::rc::Rc;
use yew::{
    component, functional::UseReducerDispatcher, html, use_effect_with, use_memo, use_reducer, use_state, Callback,
    Children, ContextProvider, Html, Properties, Reducible,
};

#[derive(Properties, PartialEq)]
pub struct DialogProviderProps {
    pub children: Children,
}

/// Provides the [`DialogService`] context and mounts a [DialogHost] that owns
/// the dialog stack. The stack state intentionally lives in `DialogHost`, so
/// pushing or popping a dialog never re-renders the application children.
#[component]
pub fn DialogProvider(props: &DialogProviderProps) -> Html {
    let service = use_state(DialogService::new);

    html! {
        <ContextProvider<DialogService> context={(*service).clone()}>
            { for props.children.iter() }

            <DialogHost service={(*service).clone()} />
        </ContextProvider<DialogService>>
    }
}

#[derive(Clone)]
struct DialogEntry {
    id: u64,
    request: DialogRequest,
}

// Entries are immutable once pushed, so the id fully determines identity.
impl PartialEq for DialogEntry {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}

#[derive(Default)]
struct DialogStack {
    next_id: u64,
    dialogs: Vec<DialogEntry>,
}

enum DialogStackAction {
    Push(DialogRequest),
    Pop(u64),
}

impl Reducible for DialogStack {
    type Action = DialogStackAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        match action {
            DialogStackAction::Push(request) => {
                let mut dialogs = self.dialogs.clone();

                dialogs.push(DialogEntry { id: self.next_id, request });

                Self { next_id: self.next_id + 1, dialogs }.into()
            }

            // Nur der oberste Dialog darf entfernt werden. Ein verspätetes
            // Pop für eine darunterliegende id tut bewusst nichts und gibt
            // den unveränderten State zurück (kein Clone, kein Re-Render).
            DialogStackAction::Pop(id) => {
                if self.dialogs.last().map(|entry| entry.id) != Some(id) {
                    return self;
                }

                let mut dialogs = self.dialogs.clone();
                dialogs.pop();

                Self { next_id: self.next_id, dialogs }.into()
            }
        }
    }
}

#[derive(Properties, PartialEq)]
struct DialogHostProps {
    service: DialogService,
}

/// Owns the dialog stack and renders one [`DialogLayer`] per stacked dialog.
/// Only this component re-renders when the stack changes.
#[component]
fn DialogHost(props: &DialogHostProps) -> Html {
    // Since Yew 0.23, `use_reducer` does not re-render when the reducer
    // returns the same `Rc`. Therefore, a no-op `Pop` (see `reduce`) has
    // no effect. Every actual stack change returns a new `Rc` and triggers
    // the required re-render.
    let dialog_stack = use_reducer(DialogStack::default);

    {
        let service = props.service.clone();
        let dispatcher = dialog_stack.dispatcher();

        use_effect_with((), move |()| {
            service.register(Callback::from(move |request: DialogRequest| {
                dispatcher.dispatch(DialogStackAction::Push(request));
            }));

            || ()
        });
    }

    let dispatcher = dialog_stack.dispatcher();
    let dialog_count = dialog_stack.dialogs.len();

    html! {
        {
            for dialog_stack
                .dialogs
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    html! {
                        <DialogLayer
                            key={entry.id}
                            entry={entry.clone()}
                            active={index + 1 == dialog_count}
                            dispatcher={dispatcher.clone()}
                        />
                    }
                })
        }
    }
}

#[derive(Properties, PartialEq)]
struct DialogLayerProps {
    entry: DialogEntry,
    active: bool,
    dispatcher: UseReducerDispatcher<DialogStack>,
}

/// Renders a single stacked dialog. The stable key on this component keeps
/// lower dialogs mounted while a dialog above them opens or closes, so their
/// enter animation never replays.
#[component]
fn DialogLayer(props: &DialogLayerProps) -> Html {
    // Der Layer löst seinen eigenen Request auf: id, request und Resolver
    // sind unveränderlich, der Dispatcher ist stabil — der Callback braucht
    // daher keinen Zugriff auf den aktuellen Stack.
    let on_confirm = {
        let id = props.entry.id;
        let request = props.entry.request.clone();
        let dispatcher = props.dispatcher.clone();

        use_memo(id, move |_| {
            Callback::from(move |result: DialogResult| {
                let resolver = match &request {
                    DialogRequest::Confirm(confirm) => confirm.resolve.borrow_mut().take(),
                    DialogRequest::Content(content) => content.resolve.borrow_mut().take(),
                };

                if let Some(resolve) = resolver {
                    // Wichtig: zuerst genau diesen Dialog vom Stack
                    // entfernen, dann erst die Future fortsetzen. Die Future
                    // darf sofort einen neuen Dialog öffnen, der dann auf
                    // dem bereits geleerten Platz landet.
                    dispatcher.dispatch(DialogStackAction::Pop(id));

                    resolve(result);
                }
            })
        })
    };

    // Nur der oberste Dialog darf Ergebnisse liefern.
    let on_confirm = if props.active { (*on_confirm).clone() } else { Callback::noop() };

    match &props.entry.request {
        DialogRequest::Confirm(confirm) => html! {
            <ConfirmDialog
                title={confirm.title.clone()}
                ok_caption={confirm.ok_caption.clone()}
                cancel_caption={confirm.cancel_caption.clone()}
                on_confirm={on_confirm}
            />
        },

        DialogRequest::Content(content) => html! {
            <ContentDialog
                content={content.content.clone()}
                actions={content.actions.clone()}
                close_on_backdrop_click={props.active && content.close_on_backdrop_click}
                on_confirm={on_confirm}
            />
        },
    }
}
