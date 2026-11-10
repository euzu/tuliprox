use crate::error::{Error, ErrorInfo, ErrorSetInfo};
use gloo_net::http::{Request, RequestBuilder, Response};
use gloo_storage::{LocalStorage, Storage};
use log::{error, warn};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use shared::utils::{bin_deserialize, bin_serialize, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON};
use std::{collections::HashMap, sync::OnceLock};
use web_sys::{window, FormData};

enum RequestMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone)]
pub struct ResponseMeta<T> {
    pub body: Option<T>,
    pub headers: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    Json,
    Cbor,
    Text,
}

enum EncodedBody {
    Text(String),
    Binary(Vec<u8>),
}

impl Encoding {
    const fn as_content_type(self) -> &'static str {
        match self {
            Self::Json => CONTENT_TYPE_JSON,
            Self::Cbor => CONTENT_TYPE_CBOR,
            Self::Text => "text/plain",
        }
    }

    fn encode<B: Serialize>(self, body: &B) -> Result<EncodedBody, Error> {
        match self {
            Self::Cbor => {
                let bytes = bin_serialize(body).map_err(|_| Error::RequestError)?;
                Ok(EncodedBody::Binary(bytes))
            }
            Self::Json => {
                let text = serde_json::to_string(body).map_err(|_| Error::RequestError)?;
                Ok(EncodedBody::Text(text))
            }
            Self::Text => {
                let value = serde_json::to_value(body).map_err(|_| Error::RequestError)?;
                match value {
                    Value::String(text) => Ok(EncodedBody::Text(text)),
                    _ => Err(Error::RequestError),
                }
            }
        }
    }
}

fn apply_encoded_body(
    request: RequestBuilder,
    body: EncodedBody,
    content_type: &'static str,
) -> Result<Request, Error> {
    let request = request.header("Content-Type", content_type);
    match body {
        EncodedBody::Text(payload) => request.body(payload),
        EncodedBody::Binary(payload) => request.body(payload),
    }
    .map_err(|err| {
        error!("Failed to attach request body: {err}");
        Error::RequestError
    })
}

fn encoding_from_content_type(content_type: &str) -> Option<Encoding> {
    if content_type.contains(CONTENT_TYPE_CBOR) {
        Some(Encoding::Cbor)
    } else if content_type.contains(CONTENT_TYPE_JSON) {
        Some(Encoding::Json)
    } else if content_type.contains("text/") {
        Some(Encoding::Text)
    } else {
        None
    }
}

fn should_decode_success_body<T>(status: u16, content_type: &str) -> bool
where
    T: 'static,
{
    status != 204
        && std::any::TypeId::of::<T>() != std::any::TypeId::of::<()>()
        && encoding_from_content_type(content_type).is_some()
}

async fn decode_response_body<T>(response: Response, encoding: Encoding) -> Result<T, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    match encoding {
        Encoding::Cbor => {
            let bytes = response.binary().await.map_err(|err| {
                error!("Failed to read response body for CBOR decode: {err}");
                Error::DeserializeError
            })?;
            bin_deserialize::<T>(&bytes).map_err(|err| {
                error!("Failed to deserialize {err}");
                Error::DeserializeError
            })
        }
        Encoding::Json => response.json::<T>().await.map_err(|_| Error::DeserializeError),
        Encoding::Text => match response.text().await {
            Ok(content) => serde_json::from_value::<T>(Value::String(content)).map_err(|_| Error::DeserializeError),
            Err(_) => Err(Error::RequestError),
        },
    }
}

async fn extract_error_message_checked(response: Response) -> Result<String, Error> {
    let content_type = response.headers().get("content-type").unwrap_or_default().to_ascii_lowercase();
    match encoding_from_content_type(&content_type) {
        Some(Encoding::Cbor) => {
            let bytes = response.binary().await.map_err(|_| Error::DeserializeError)?;
            let data = bin_deserialize::<ErrorInfo>(&bytes).map_err(|_| Error::DeserializeError)?;
            Ok(data.error)
        }
        Some(Encoding::Json) => {
            let data = response.json::<ErrorInfo>().await.map_err(|_| Error::DeserializeError)?;
            Ok(data.error)
        }
        _ => Ok(response.text().await.unwrap_or_default()),
    }
}

async fn extract_error_message(response: Response) -> String {
    extract_error_message_checked(response).await.unwrap_or_default()
}

/// The one status every request path turns into `Error::Unauthorized`, before any other status handling.
fn reject_unauthorized(status: u16) -> Result<(), Error> {
    if status == 401 {
        Err(Error::Unauthorized)
    } else {
        Ok(())
    }
}

fn unexpected_response_error(message: String) -> Error {
    if message.trim().is_empty() {
        Error::RequestError
    } else {
        Error::HttpResponse(message)
    }
}

/// Maps a non-success HTTP status onto the shared [`Error`] vocabulary.
///
/// `None` means the status carries no generic error, either because the caller decodes a
/// typed body (`422`) or because the status is not part of the protocol.
fn status_error(status: u16, message: &str) -> Option<Error> {
    let text = || {
        if message.trim().is_empty() {
            status.to_string()
        } else {
            message.to_string()
        }
    };
    match status {
        400 => Some(Error::BadRequest(text())),
        401 => Some(Error::Unauthorized),
        403 => Some(Error::Forbidden(if message.trim().is_empty() {
            "Forbidden".to_string()
        } else {
            message.to_string()
        })),
        404 => Some(Error::NotFound),
        409 => Some(Error::Conflict(if message.trim().is_empty() {
            "Configuration conflict (409)".to_string()
        } else {
            message.to_string()
        })),
        412 => Some(Error::PreconditionFailed(message.to_string())),
        428 => Some(Error::PreconditionRequired(if message.trim().is_empty() {
            "Missing precondition header (428)".to_string()
        } else {
            message.to_string()
        })),
        500 => Some(Error::InternalServerError(if message.trim().is_empty() {
            "Internal Server Error".to_string()
        } else {
            message.to_string()
        })),
        _ => None,
    }
}

const TOKEN_KEY: &str = "tuliprox.token";
pub fn get_token() -> Option<String> { LocalStorage::get(TOKEN_KEY).ok() }

pub fn set_token(token: Option<&str>) {
    if let Some(t) = token {
        if let Err(err) = LocalStorage::set(TOKEN_KEY, String::from(t)) {
            warn!("failed to set token in localStorage: {err:?}");
        }
    } else {
        LocalStorage::delete(TOKEN_KEY);
    }
}

const DUMMY_TOKEN: &str = "eyJraWQiOiJkZWZhdWx0IiwiYWxnIjoiUlMyNTYifQ.eyJsb2dpbiI6ImR1bW15In0.WWzZP0hICmJeIgMLVYNOpayriEC08J_lYssk9z8GglHXfZ6oJUDv3svlJDA8sQG025VA_LR5UzyyiWeQDCdpWyrCI_nI2Xd-3ga3JwWtxHE9NWFalgq0Q9jjxoB4LYWCXsAkqoZqk6s7b3F5Fi_h5oYHfwM4h8hXEbrgnJ_Z1wpSc7HNh6SUnOllxcaJOxYlRrlUn3XulSSf2NhHe3XotvFguiIV1-RIns3cSIL29bvMUEFw84w7BfJn-joynZsWlfJBvzyOiuDqduXa0deH7b962unM2wPpbvTgliJhFFOUBHClRhBOmoo0cijuMZB4K7NjgjGmU5eVfHG6pVWs_b0ikS4V_P6RJcNS6Alcc_HB_YXv0yCD3pjcBbuRXAskivEhgXuecdRMGQgohAhXplLuu5SR0K6Bcrt7UFFnBi2qN6fbw1i4s8PDXqiTu4rIg9agCkVNfplRvj8Szl6egF0Vd1TN1WGEarkdINEUyfNQkAihFY5BKxfaPun1-a0VydRMZElu6VzrrUMxXt4T7zybuJZI63C3mKLEHZixdSC76c9AE-zGom5LZYE4mqwd4dW3QHtWFZgGZiL9C_VBIf63WzjTYhWVuO2U8O9bsKkSEl5L-Ww9j8ccDHp5nc7y6yUgSYd600TBRI7WblFovLsl2tjElvUqfJZhj6JmX_Q";

// The Authorization header is for the Backend Authenticator mandatory.
// If we don't set a dummy token the backend api can't be called.
pub fn check_dummy_token() {
    if get_token().is_none() {
        set_token(Some(DUMMY_TOKEN));
    }
}

/// Builds and sends a request; the payload is attached last, after all headers are set.
async fn send_request<B>(
    method: RequestMethod,
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    request_headers: Option<&[(String, String)]>,
) -> Result<Response, Error>
where
    B: Serialize + std::fmt::Debug,
{
    let c_type = content_type.unwrap_or(Encoding::Json);
    let r_type = response_type.unwrap_or(Encoding::Json);
    // `RequestBuilder::body` consumes the builder and hands back a `Request`, which
    // no longer accepts headers. So the payload is encoded up front but only attached
    // once Authorization, the caller's extra headers and Accept have all been set.
    let (mut request, encoded) = match method {
        RequestMethod::Get => (Request::get(url), None),
        RequestMethod::Post => (Request::post(url), Some(c_type.encode(&body)?)),
        RequestMethod::Put => (Request::put(url), Some(c_type.encode(&body)?)),
        RequestMethod::Patch => (Request::patch(url), Some(c_type.encode(&body)?)),
        RequestMethod::Delete => (Request::delete(url), None),
    };
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    if let Some(extra_headers) = request_headers {
        for (key, value) in extra_headers {
            request = request.header(key, value);
        }
    }

    request = request.header("Accept", r_type.as_content_type());

    let sent = match encoded {
        Some(encoded) => apply_encoded_body(request, encoded, c_type.as_content_type())?.send().await,
        None => request.send().await,
    };
    sent.map_err(|e| {
        error!("{e}");
        Error::RequestError
    })
}

/// build all kinds of http request: post/get/delete etc.
async fn request<B, T>(
    method: RequestMethod,
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    request_headers: Option<&[(String, String)]>,
    response_header_keys: Option<&[&str]>,
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    let r_type = response_type.unwrap_or(Encoding::Json);
    let sent = send_request(method, url, body, content_type, response_type, request_headers).await;

    match sent {
        Ok(response) => {
            let status = response.status();
            reject_unauthorized(status)?;
            let mut response_headers = HashMap::new();
            if let Some(header_keys) = response_header_keys {
                for key in header_keys {
                    if let Some(value) = response.headers().get(key) {
                        response_headers.insert((*key).to_string(), value);
                    }
                }
            }
            match status {
                200 | 201 | 202 | 205 | 206 => {
                    let content_type = response.headers().get("content-type").unwrap_or_default().to_ascii_lowercase();
                    if !should_decode_success_body::<T>(status, &content_type) {
                        let _ = response.binary().await;
                        return Ok(ResponseMeta { body: None, headers: response_headers });
                    }
                    let decode_encoding = encoding_from_content_type(&content_type).unwrap_or(r_type);
                    let decoded = decode_response_body::<T>(response, decode_encoding).await?;
                    Ok(ResponseMeta { body: Some(decoded), headers: response_headers })
                }
                204 => Ok(ResponseMeta { body: None, headers: response_headers }),
                422 => {
                    let ct = response.headers().get("content-type").unwrap_or_default();
                    let is_json = ct.contains(CONTENT_TYPE_JSON);
                    let is_bin = !is_json && ct.contains(CONTENT_TYPE_CBOR);
                    let data: Result<ErrorSetInfo, _> = if is_bin {
                        match response.binary().await {
                            Ok(bytes) => bin_deserialize::<ErrorSetInfo>(&bytes).map_err(|_| Error::DeserializeError),
                            Err(_) => Err(Error::DeserializeError),
                        }
                    } else {
                        response.json::<ErrorSetInfo>().await.map_err(|_| Error::DeserializeError)
                    };

                    if let Ok(data) = data {
                        Err(Error::UnprocessableEntity(data))
                    } else {
                        Err(Error::DeserializeError)
                    }
                }
                _ => {
                    let message =
                        if matches!(status, 401 | 404) { String::new() } else { extract_error_message(response).await };
                    match status_error(status, &message) {
                        Some(error) => Err(error),
                        None => Err(unexpected_response_error(message)),
                    }
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Sends a JSON request and hands back status and text body for callers that handle non-success statuses
/// themselves. A 401 is not handed back: it is the same `Error::Unauthorized` every other request path reports.
async fn request_method_raw<B>(method: RequestMethod, url: &str, body: B) -> Result<RawResponse, Error>
where
    B: Serialize + std::fmt::Debug,
{
    let response = send_request(method, url, body, Some(Encoding::Json), Some(Encoding::Json), None).await?;
    let status = response.status();
    reject_unauthorized(status)?;
    let body = response.text().await.map_err(|_| Error::RequestError)?;
    Ok(RawResponse { status, body })
}

/// Put request that hands back every status with its body instead of mapping errors.
pub async fn request_put_raw<B>(url: &str, body: B) -> Result<RawResponse, Error>
where
    B: Serialize + std::fmt::Debug,
{
    request_method_raw(RequestMethod::Put, url, body).await
}

/// Post request that hands back every status with its body instead of mapping errors.
pub async fn request_post_raw<B>(url: &str, body: B) -> Result<RawResponse, Error>
where
    B: Serialize + std::fmt::Debug,
{
    request_method_raw(RequestMethod::Post, url, body).await
}

/// Delete request that hands back every status with its body instead of mapping errors.
pub async fn request_delete_raw(url: &str) -> Result<RawResponse, Error> {
    request_method_raw(RequestMethod::Delete, url, ()).await
}

/// Delete request
pub async fn request_delete<T>(
    url: &str,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Delete, url, (), content_type, response_type, None, None).await.map(|response| response.body)
}

/// Get request
pub async fn request_get<T>(
    url: &str,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Get, url, (), content_type, response_type, None, None).await.map(|response| response.body)
}

/// A response the caller decodes itself: the status and the body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    pub status: u16,
    pub body: String,
}

/// `POST` a `multipart/form-data` body and hand back the status and the body
/// text, so the caller can read the error body of a `422`.
///
/// No `Content-Type` is set: the browser derives it from the `FormData`,
/// including the boundary the body needs. A header set here would drop it.
pub async fn request_post_form(url: &str, form: FormData) -> Result<RawResponse, Error> {
    let mut request = Request::post(url).header("Accept", CONTENT_TYPE_JSON);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    let request = request.body(form).map_err(|err| {
        error!("Failed to attach the form body: {err}");
        Error::RequestError
    })?;
    let response = request.send().await.map_err(|err| {
        error!("{err}");
        Error::RequestError
    })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Ok(RawResponse { status, body })
}

/// Send `request` with the bearer token and an optional JSON body, and hand
/// back the status and the body text, whatever the status. For callers that
/// must read an error body the typed helpers fold away: a `422` with field
/// problems, or the exact status a plugin frame is told.
pub async fn request_raw(request: RequestBuilder, json_body: Option<String>) -> Result<RawResponse, Error> {
    let mut request = request.header("Accept", CONTENT_TYPE_JSON);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    let response = match json_body {
        Some(payload) => {
            request
                .header("Content-Type", CONTENT_TYPE_JSON)
                .body(payload)
                .map_err(|_| Error::RequestError)?
                .send()
                .await
        }
        None => request.send().await,
    }
    .map_err(|err| {
        error!("{err}");
        Error::RequestError
    })?;
    Ok(RawResponse { status: response.status(), body: response.text().await.unwrap_or_default() })
}

pub async fn request_get_binary(url: &str, if_match: Option<&str>) -> Result<Vec<u8>, Error> {
    let mut request = Request::get(url);
    if let Some(revision) = if_match {
        request = request.header("If-Match", revision);
    }
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }

    let response = request.send().await.map_err(|err| {
        error!("{err}");
        Error::RequestError
    })?;

    reject_unauthorized(response.status())?;
    let status = response.status();
    match status {
        200 | 205 | 206 => response.binary().await.map_err(|err| {
            error!("Failed to read binary response body: {err}");
            Error::RequestError
        }),
        _ => {
            let message =
                if matches!(status, 401 | 404) { String::new() } else { extract_error_message(response).await };
            match status_error(status, &message) {
                Some(error) => Err(error),
                None => Err(unexpected_response_error(message)),
            }
        }
    }
}

pub async fn request_get_meta<T>(
    url: &str,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    response_header_keys: &[&str],
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Get, url, (), content_type, response_type, None, Some(response_header_keys)).await
}

// pub async fn request_get_api<T>(url: &str) -> Result<T, Error>
// where
//     T: DeserializeOwned + 'static + std::fmt::Debug,
// {
//     request(RequestMethod::Get, format!("{API_ROOT}{url}").as_str(), ()).await
// }

/// Post request with a body
pub async fn request_post<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Post, url, body, content_type, response_type, None, None).await.map(|response| response.body)
}

pub async fn request_post_meta<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    request_headers: Option<&[(String, String)]>,
    response_header_keys: &[&str],
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Post, url, body, content_type, response_type, request_headers, Some(response_header_keys))
        .await
}

/// A failed write request whose failure body is decoded by the caller.
///
/// Endpoints that answer a failure with a typed body (409 conflict, 422 field errors) cannot
/// use [`request`], which maps every non-success status onto [`Error`] and drops the body.
/// This keeps the auth header, the dummy token and the status mapping in one place while
/// leaving the body to the caller.
#[derive(Debug)]
pub struct WriteFailure<E> {
    /// `0` when the request never reached the server.
    pub status: u16,
    /// Shared error mapping for the status, `None` for statuses decoded by the caller.
    pub error: Option<Error>,
    /// Decoded failure body, when the payload could be deserialized into `E`.
    pub body: Option<E>,
    /// Human readable failure text (the decoded `error` field when the body carried one).
    pub message: String,
}

impl<E> WriteFailure<E> {
    fn transport(error: Error) -> Self { Self { status: 0, error: Some(error), body: None, message: String::new() } }

    /// Error text for callers that cannot use the typed body.
    #[must_use]
    pub fn into_message(self) -> String { self.error.map_or(self.message, |error| error.to_string()) }
}

/// JSON `POST` whose failure body is deserialized into `E` instead of being folded into a string.
pub async fn request_post_with_error_body<B, T, E>(url: &str, body: B) -> Result<Option<T>, WriteFailure<E>>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
    E: DeserializeOwned,
{
    check_dummy_token();

    let encoded = Encoding::Json.encode(&body).map_err(WriteFailure::transport)?;
    let mut request = Request::post(url);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    request = request.header("Accept", Encoding::Json.as_content_type());
    let request =
        apply_encoded_body(request, encoded, Encoding::Json.as_content_type()).map_err(WriteFailure::transport)?;

    let response = request.send().await.map_err(|err| {
        error!("{err}");
        WriteFailure::transport(Error::RequestError)
    })?;

    let status = response.status();
    if (200..300).contains(&status) {
        let content_type = response.headers().get("content-type").unwrap_or_default().to_ascii_lowercase();
        if !should_decode_success_body::<T>(status, &content_type) {
            let _ = response.binary().await;
            return Ok(None);
        }
        let encoding = encoding_from_content_type(&content_type).unwrap_or(Encoding::Json);
        return decode_response_body::<T>(response, encoding).await.map(Some).map_err(WriteFailure::transport);
    }

    let bytes = response.binary().await.unwrap_or_default();
    let message = serde_json::from_slice::<ErrorInfo>(&bytes)
        .map_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_string(), |info| info.error);
    let body = serde_json::from_slice::<E>(&bytes).ok();
    Err(WriteFailure { status, error: status_error(status, &message), body, message })
}

/// Put request with a body
pub async fn request_put<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Put, url, body, content_type, response_type, None, None).await.map(|response| response.body)
}

pub async fn request_put_meta<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    request_headers: Option<&[(String, String)]>,
    response_header_keys: &[&str],
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Put, url, body, content_type, response_type, request_headers, Some(response_header_keys))
        .await
}

/// Patch request with a body
pub async fn request_patch<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Patch, url, body, content_type, response_type, None, None)
        .await
        .map(|response| response.body)
}

pub async fn request_patch_meta<B, T>(
    url: &str,
    body: B,
    content_type: Option<Encoding>,
    response_type: Option<Encoding>,
    request_headers: Option<&[(String, String)]>,
    response_header_keys: &[&str],
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Patch, url, body, content_type, response_type, request_headers, Some(response_header_keys))
        .await
}

/// Set limit for pagination
pub fn limit(count: u32, p: u32) -> String {
    let offset = if p > 0 { p * count } else { 0 };
    format!("limit={count}&offset={offset}")
}

static BASE_HREF: OnceLock<String> = OnceLock::new();

pub fn get_base_href() -> String {
    BASE_HREF
        .get_or_init(|| {
            let mut href = window()
                .and_then(|w| w.document())
                .and_then(|doc| doc.query_selector("base").ok().flatten())
                .and_then(|base| base.get_attribute("href"))
                .map_or_else(|| "/".to_owned(), |s| s.trim().to_owned());

            if !href.ends_with('/') {
                href.push('/');
            }

            href
        })
        .clone()
}

pub async fn request_delete_meta<T>(
    url: &str,
    request_headers: &[(String, String)],
    response_header_keys: &[&str],
) -> Result<ResponseMeta<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Delete, url, (), None, None, Some(request_headers), Some(response_header_keys)).await
}

#[cfg(test)]
mod tests {
    use super::{reject_unauthorized, should_decode_success_body, unexpected_response_error};
    use crate::error::Error;

    #[test]
    fn accepted_json_response_with_body_is_decoded() {
        assert!(should_decode_success_body::<serde_json::Value>(202, "application/json"));
    }

    #[test]
    fn accepted_response_without_decodable_content_type_is_not_decoded() {
        assert!(!should_decode_success_body::<serde_json::Value>(202, ""));
    }

    #[test]
    fn no_content_and_unit_responses_are_not_decoded() {
        assert!(!should_decode_success_body::<serde_json::Value>(204, "application/json"));
        assert!(!should_decode_success_body::<()>(202, "application/json"));
    }

    #[test]
    fn only_401_is_rejected_as_unauthorized() {
        assert_eq!(reject_unauthorized(401), Err(Error::Unauthorized));
        for status in [200, 204, 400, 403, 404, 409, 422, 423, 500] {
            assert_eq!(reject_unauthorized(status), Ok(()));
        }
    }

    #[test]
    fn unexpected_http_status_preserves_recording_error_code() {
        assert_eq!(
            unexpected_response_error("recording_disabled".to_string()),
            Error::HttpResponse("recording_disabled".to_string())
        );
    }
}
