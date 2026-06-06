use web_sys::MouseEvent;
use yew::Callback;

pub(crate) fn prevent_default_and_stop<E, F>(emit: F) -> Callback<MouseEvent>
where
    E: 'static,
    F: Fn(MouseEvent) -> E + 'static,
{
    Callback::from(move |event: MouseEvent| {
        event.prevent_default();
        event.stop_propagation();
        let payload = emit(event);
        let _ = payload;
    })
}
