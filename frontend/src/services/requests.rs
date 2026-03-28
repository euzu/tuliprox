use crate::error::{Error, ErrorInfo, ErrorSetInfo};
use gloo_storage::{LocalStorage, Storage};
use log::{error, warn};
use reqwasm::http::{Request, Response};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use shared::utils::{bin_deserialize, bin_serialize, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON};
use std::{collections::HashMap, sync::OnceLock};
use web_sys::window;

enum RequestMethod {
    Get,
    Post,
    Put,
    // Patch,
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

fn apply_encoded_body(request: Request, body: EncodedBody, content_type: &'static str) -> Request {
    match body {
        EncodedBody::Text(payload) => request.body(payload).header("Content-Type", content_type),
        EncodedBody::Binary(payload) => request.body(payload).header("Content-Type", content_type),
    }
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
    let c_type = content_type.unwrap_or(Encoding::Json);
    let r_type = response_type.unwrap_or(Encoding::Json);
    let mut request = match method {
        RequestMethod::Get => Request::get(url),
        RequestMethod::Post => {
            let encoded = c_type.encode(&body)?;
            apply_encoded_body(Request::post(url), encoded, c_type.as_content_type())
        }
        RequestMethod::Put => {
            let encoded = c_type.encode(&body)?;
            apply_encoded_body(Request::put(url), encoded, c_type.as_content_type())
        }
        // RequestMethod::PATCH =>  Request::patch(&url).body(serde_json::to_string(&body).unwrap()),
        RequestMethod::Delete => Request::delete(url),
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

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let mut response_headers = HashMap::new();
            if let Some(header_keys) = response_header_keys {
                for key in header_keys {
                    if let Some(value) = response.headers().get(key) {
                        response_headers.insert((*key).to_string(), value);
                    }
                }
            }
            match status {
                200 | 205 | 206 => {
                    let content_type = response.headers().get("content-type").unwrap_or_default().to_ascii_lowercase();
                    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>() {
                        // `T = ()` valid
                        let _ = response.binary().await;
                        return Ok(ResponseMeta { body: None, headers: response_headers });
                    }
                    let decode_encoding = encoding_from_content_type(&content_type).unwrap_or(r_type);
                    let decoded = decode_response_body::<T>(response, decode_encoding).await?;
                    Ok(ResponseMeta { body: Some(decoded), headers: response_headers })
                }
                201 | 202 | 204 => Ok(ResponseMeta { body: None, headers: response_headers }),
                400 => {
                    let message = extract_error_message(response).await;
                    if message.trim().is_empty() {
                        Err(Error::BadRequest("400".to_string()))
                    } else {
                        Err(Error::BadRequest(message))
                    }
                }
                401 => Err(Error::Unauthorized),
                403 => {
                    let message = extract_error_message(response).await;
                    if message.trim().is_empty() {
                        Err(Error::Forbidden("Forbidden".to_string()))
                    } else {
                        Err(Error::Forbidden(message))
                    }
                }
                404 => Err(Error::NotFound),
                409 => {
                    let message = extract_error_message(response).await;
                    if message.trim().is_empty() {
                        Err(Error::Conflict("Configuration conflict (409)".to_string()))
                    } else {
                        Err(Error::Conflict(message))
                    }
                }
                428 => {
                    let message = extract_error_message(response).await;
                    if message.trim().is_empty() {
                        Err(Error::PreconditionRequired("Missing precondition header (428)".to_string()))
                    } else {
                        Err(Error::PreconditionRequired(message))
                    }
                }
                500 => {
                    let message = extract_error_message(response).await;
                    if message.trim().is_empty() {
                        Err(Error::InternalServerError("Internal Server Error".to_string()))
                    } else {
                        Err(Error::InternalServerError(message))
                    }
                }
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
                _ => Err(Error::RequestError),
            }
        }
        Err(e) => {
            error!("{e}");
            Err(Error::RequestError)
        }
    }
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

pub async fn request_get_binary(url: &str) -> Result<Vec<u8>, Error> {
    let mut request = Request::get(url);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }

    let response = request.send().await.map_err(|err| {
        error!("{err}");
        Error::RequestError
    })?;

    match response.status() {
        200 | 205 | 206 => response.binary().await.map_err(|err| {
            error!("Failed to read binary response body: {err}");
            Error::RequestError
        }),
        400 => {
            let message = extract_error_message(response).await;
            if message.trim().is_empty() {
                Err(Error::BadRequest("400".to_string()))
            } else {
                Err(Error::BadRequest(message))
            }
        }
        401 => Err(Error::Unauthorized),
        403 => {
            let message = extract_error_message(response).await;
            if message.trim().is_empty() {
                Err(Error::Forbidden("Forbidden".to_string()))
            } else {
                Err(Error::Forbidden(message))
            }
        }
        404 => Err(Error::NotFound),
        409 => {
            let message = extract_error_message(response).await;
            if message.trim().is_empty() {
                Err(Error::Conflict("Configuration conflict (409)".to_string()))
            } else {
                Err(Error::Conflict(message))
            }
        }
        428 => {
            let message = extract_error_message(response).await;
            if message.trim().is_empty() {
                Err(Error::PreconditionRequired("Missing precondition header (428)".to_string()))
            } else {
                Err(Error::PreconditionRequired(message))
            }
        }
        500 => {
            let message = extract_error_message(response).await;
            if message.trim().is_empty() {
                Err(Error::InternalServerError("Internal Server Error".to_string()))
            } else {
                Err(Error::InternalServerError(message))
            }
        }
        _ => Err(Error::RequestError),
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
