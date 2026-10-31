use crate::error::Error;

#[cfg(target_arch = "wasm32")]
pub fn trigger_browser_download(data: &[u8], filename: &str, mime_type: &str) -> Result<(), Error> {
    use wasm_bindgen::JsCast;
    let uint8_array = js_sys::Uint8Array::from(data);
    let parts = js_sys::Array::new();
    parts.push(&uint8_array);

    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime_type);

    let blob =
        web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).map_err(|_| Error::RequestError)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|_| Error::RequestError)?;

    let document = web_sys::window().and_then(|w| w.document()).ok_or(Error::RequestError)?;

    let link: web_sys::HtmlAnchorElement =
        document.create_element("a").map_err(|_| Error::RequestError)?.dyn_into().map_err(|_| Error::RequestError)?;

    link.set_href(&url);
    link.set_download(filename);
    link.click();

    // Defer revoke by ~1s to prevent browsers from aborting the download in flight
    let url_cleanup = url.clone();
    gloo_timers::callback::Timeout::new(1000, move || {
        let _ = web_sys::Url::revoke_object_url(&url_cleanup);
    })
    .forget();

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn trigger_browser_download(_data: &[u8], _filename: &str, _mime_type: &str) -> Result<(), Error> { Ok(()) }
