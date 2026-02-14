use crate::error::{Error, ErrorInfo, ErrorSetInfo};
use gloo_storage::{LocalStorage, Storage};
use log::error;
use reqwasm::http::Request;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use shared::utils::{bin_deserialize, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON};
use web_sys::window;

enum RequestMethod {
    Get,
    Post,
    Put,
    // Patch,
    Delete,
}

const TOKEN_KEY: &str = "tuliprox.token";
pub fn get_token() -> Option<String> { LocalStorage::get(TOKEN_KEY).ok() }

pub fn set_token(token: Option<&str>) {
    if let Some(t) = token {
        LocalStorage::set(TOKEN_KEY, String::from(t)).expect("failed to set");
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
    content_type: Option<String>,
    response_type: Option<String>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    let c_type = content_type.as_ref().map_or(CONTENT_TYPE_JSON, |c| c.as_str());
    let r_type = response_type.as_ref().map_or(CONTENT_TYPE_JSON, |c| c.as_str());
    let mut request = match method {
        RequestMethod::Get => Request::get(url),
        RequestMethod::Post => {
            let json = serde_json::to_string(&body).map_err(|_| Error::RequestError)?;
            Request::post(url).body(json).header("Content-Type", c_type)
        }
        RequestMethod::Put => {
            let json = serde_json::to_string(&body).map_err(|_| Error::RequestError)?;
            Request::put(url).body(json).header("Content-Type", c_type)
        }
        // RequestMethod::PATCH =>  Request::patch(&url).body(serde_json::to_string(&body).unwrap()),
        RequestMethod::Delete => Request::delete(url),
    };
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }

    if r_type.contains(CONTENT_TYPE_CBOR) {
        request = request.header("Accept", CONTENT_TYPE_CBOR);
    }

    match request.send().await {
        Ok(response) => {
            match response.status() {
                200 | 205 | 206 => {
                    let content_type = response.headers().get("content-type").unwrap_or(r_type.to_string());
                    let is_json = content_type.contains(CONTENT_TYPE_JSON);
                    let is_bin = !is_json && content_type.contains(CONTENT_TYPE_CBOR);
                    if (is_json || is_bin) && std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>() {
                        // `T = ()` valid
                        let _ = response.binary().await;
                        return Ok(None);
                    }

                    if is_bin {
                        match response.binary().await {
                            Ok(bytes) => match bin_deserialize::<T>(&bytes) {
                                Ok(data) => Ok(Some(data)),
                                Err(err) => {
                                    error!("Failed to deserialize {err}");
                                    Err(Error::DeserializeError)
                                }
                            },
                            Err(err) => {
                                error!("Failed to deserialize {err}");
                                Err(Error::DeserializeError)
                            }
                        }
                    } else if is_json {
                        let data: Result<T, _> = response.json::<T>().await;
                        if let Ok(data) = data {
                            Ok(Some(data))
                        } else {
                            Err(Error::DeserializeError)
                        }
                    } else {
                        match response.text().await {
                            Ok(content) => match serde_json::from_value::<T>(Value::String(content)) {
                                Ok(parsed) => Ok(Some(parsed)),
                                Err(_err) => Err(Error::DeserializeError),
                            },
                            Err(_err) => Err(Error::RequestError),
                        }
                    }
                }
                201 | 202 | 204 => Ok(None),
                400 => {
                    let ct = response.headers().get("content-type").unwrap_or_default();
                    let is_json = ct.contains(CONTENT_TYPE_JSON);
                    let is_bin = !is_json && ct.contains(CONTENT_TYPE_CBOR);
                    let data: Result<ErrorInfo, _> = if is_bin {
                        match response.binary().await {
                            Ok(bytes) => bin_deserialize::<ErrorInfo>(&bytes).map_err(|_| Error::DeserializeError),
                            Err(_) => Err(Error::DeserializeError),
                        }
                    } else {
                        response.json::<ErrorInfo>().await.map_err(|_| Error::DeserializeError)
                    };

                    if let Ok(data) = data {
                        Err(Error::BadRequest(data.error))
                    } else {
                        Err(Error::BadRequest("400".to_string()))
                    }
                }
                401 => Err(Error::Unauthorized),
                403 => Err(Error::Forbidden),
                404 => Err(Error::NotFound),
                500 => Err(Error::InternalServerError),
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
    content_type: Option<String>,
    response_type: Option<String>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Delete, url, (), content_type, response_type).await
}

/// Get request
pub async fn request_get<T>(
    url: &str,
    content_type: Option<String>,
    response_type: Option<String>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
{
    request(RequestMethod::Get, url, (), content_type, response_type).await
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
    content_type: Option<String>,
    response_type: Option<String>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Post, url, body, content_type, response_type).await
}

/// Put request with a body
pub async fn request_put<B, T>(
    url: &str,
    body: B,
    content_type: Option<String>,
    response_type: Option<String>,
) -> Result<Option<T>, Error>
where
    T: DeserializeOwned + 'static + std::fmt::Debug,
    B: Serialize + std::fmt::Debug,
{
    request(RequestMethod::Put, url, body, content_type, response_type).await
}

/// Set limit for pagination
pub fn limit(count: u32, p: u32) -> String {
    let offset = if p > 0 { p * count } else { 0 };
    format!("limit={count}&offset={offset}")
}

pub fn get_base_href() -> String {
    let mut href = window()
        .and_then(|w| w.document())
        .and_then(|doc| doc.query_selector("base").ok().flatten())
        .and_then(|base| base.get_attribute("href"))
        .map_or_else(|| "/".to_owned(), |s| s.trim().to_owned());

    if !href.ends_with('/') {
        href.push('/');
    }

    href
}
