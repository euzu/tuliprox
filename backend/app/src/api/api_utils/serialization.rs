use super::{try_unwrap_body, API_STREAM_CHUNK_SIZE};
use axum::{
    body::Body,
    http::{header, Response},
    response::IntoResponse,
};
use bytes::{Bytes, BytesMut};
use futures::{stream, Stream, StreamExt};
use log::warn;
use shared::utils::{bin_serialize, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON};
use std::convert::Infallible;

pub fn stream_json_or_bin_response<P>(
    accept: Option<&str>,
    data: Box<dyn Iterator<Item = P> + Send>,
) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array(data);
    }
    stream_json_array(data)
}

pub fn stream_json_or_bin_response_stream<P, S>(accept: Option<&str>, data: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array_stream(data);
    }
    stream_json_array_stream(data)
}

pub fn stream_json_or_bin_response_try_stream<P, S, E>(accept: Option<&str>, data: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    if accept.is_some_and(|value| value.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array_try_stream(data);
    }
    stream_json_array_try_stream(data)
}

pub fn stream_json_array<P>(iter: Box<dyn Iterator<Item = P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold((iter, true), |(mut iter, first)| async move {
        match iter.next() {
            Some(item) => {
                let mut json = String::new();
                if !first {
                    json.push(',');
                }
                let element = serde_json::to_string(&item).ok()?;
                json.push_str(&element);
                Some((Ok::<Bytes, Infallible>(Bytes::from(json)), (iter, false)))
            }
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

pub fn stream_bin_array<P>(iter: Box<dyn Iterator<Item = P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold(iter, |mut iter| async move {
        match iter.next() {
            Some(item) => {
                match bin_serialize(&item) {
                    Ok(buf) => Some((Ok::<Bytes, Infallible>(Bytes::from(buf)), iter)),
                    Err(err) => {
                        warn!("CBOR serialization error in stream: {err}");
                        Some((Ok::<Bytes, Infallible>(Bytes::new()), iter)) // skip errors, continue
                    }
                }
            }
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async {
            // CBOR: start indefinite-length array
            Ok::<_, Infallible>(Bytes::from_static(&[0x9f]))
        })
        .chain(stream)
        .chain(stream::once(async {
            // CBOR: end indefinite-length array
            Ok::<_, Infallible>(Bytes::from_static(&[0xff]))
        })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

pub fn stream_json_array_stream<P, S>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    let stream = stream::unfold((stream, true), |(mut stream, first)| async move {
        match stream.next().await {
            Some(item) => {
                let mut json = String::new();
                if !first {
                    json.push(',');
                }
                let element = serde_json::to_string(&item).ok()?;
                json.push_str(&element);
                Some((Ok::<Bytes, Infallible>(Bytes::from(json)), (stream, false)))
            }
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

pub(super) fn stream_json_array_try_stream<P, S, E>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stream = stream::unfold((stream, true, false), |(mut stream, first, failed)| async move {
        if failed {
            return None;
        }
        match stream.next().await {
            Some(Ok(item)) => {
                let serialized = serde_json::to_vec(&item).map_err(|error| error.to_string());
                let bytes = serialized.map(|serialized| {
                    if first {
                        Bytes::from(serialized)
                    } else {
                        let mut framed = Vec::with_capacity(serialized.len() + 1);
                        framed.push(b',');
                        framed.extend_from_slice(&serialized);
                        Bytes::from(framed)
                    }
                });
                let failed = bytes.is_err();
                Some((bytes, (stream, false, failed)))
            }
            Some(Err(error)) => Some((Err(error.to_string()), (stream, first, true))),
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, String>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, String>(Bytes::from_static(b"]")) })),
    ));
    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

pub fn stream_bin_array_stream<P, S>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    let stream = stream::unfold(stream, |mut stream| async move {
        match stream.next().await {
            Some(item) => match bin_serialize(&item) {
                Ok(buf) => Some((Ok::<Bytes, Infallible>(Bytes::from(buf)), stream)),
                Err(err) => {
                    warn!("CBOR serialization error in stream: {err}");
                    Some((Ok::<Bytes, Infallible>(Bytes::new()), stream))
                }
            },
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0x9f])) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0xff])) })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

pub(super) fn stream_bin_array_try_stream<P, S, E>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stream = stream::unfold((stream, false), |(mut stream, failed)| async move {
        if failed {
            return None;
        }
        match stream.next().await {
            Some(Ok(item)) => {
                let bytes = bin_serialize(&item).map(Bytes::from).map_err(|error| error.to_string());
                let failed = bytes.is_err();
                Some((bytes, (stream, failed)))
            }
            Some(Err(error)) => Some((Err(error.to_string()), (stream, true))),
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, String>(Bytes::from_static(&[0x9f])) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, String>(Bytes::from_static(&[0xff])) })),
    ));
    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

pub(crate) fn coalesce_byte_stream<S, E>(stream: S) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    stream::unfold((Box::pin(stream), None, false), |(mut stream, pending_error, finished)| async move {
        if let Some(error) = pending_error {
            return Some((Err(error), (stream, None, true)));
        }
        if finished {
            return None;
        }

        let mut chunk = BytesMut::with_capacity(API_STREAM_CHUNK_SIZE);
        loop {
            match stream.next().await {
                Some(Ok(bytes)) if chunk.is_empty() && bytes.len() >= API_STREAM_CHUNK_SIZE => {
                    return Some((Ok(bytes), (stream, None, false)));
                }
                Some(Ok(bytes)) => {
                    chunk.extend_from_slice(&bytes);
                    if chunk.len() >= API_STREAM_CHUNK_SIZE {
                        return Some((Ok(chunk.freeze()), (stream, None, false)));
                    }
                }
                Some(Err(error)) if chunk.is_empty() => {
                    return Some((Err(error), (stream, None, true)));
                }
                Some(Err(error)) => {
                    return Some((Ok(chunk.freeze()), (stream, Some(error), false)));
                }
                None if chunk.is_empty() => return None,
                None => return Some((Ok(chunk.freeze()), (stream, None, true))),
            }
        }
    })
    .fuse()
}
