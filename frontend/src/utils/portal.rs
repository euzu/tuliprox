use yew::{create_portal, Html};

/// Renders `content` below `<body>`, so the backdrop of a dialog is not trapped in the stacking context of the
/// panel or card that opens it. Falls back to inline rendering without a document body.
pub fn in_body(content: Html) -> Html {
    match web_sys::window().and_then(|window| window.document()).and_then(|document| document.body()) {
        Some(body) => create_portal(content, body.into()),
        None => content,
    }
}
