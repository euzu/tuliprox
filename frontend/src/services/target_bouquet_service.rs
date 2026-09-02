use crate::{
    error::Error,
    services::{get_base_href, get_token, request_delete, request_get, request_put, Encoding},
};
use gloo_net::http::Request;
use js_sys::{Reflect, Uint8Array};
use shared::{
    model::{PlaylistClusterBouquetDto, TargetBouquetStreamEventDto, TargetBouquetTargetDto},
    utils::concat_path_leading_slash,
};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

const MAX_STREAM_EVENT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct JsonArrayEventDecoder {
    buffer: Vec<u8>,
    cursor: usize,
    value_start: Option<usize>,
    depth: usize,
    in_string: bool,
    escaped: bool,
    started: bool,
    finished: bool,
}

impl JsonArrayEventDecoder {
    fn push<F>(&mut self, bytes: &[u8], on_event: &mut F) -> Result<(), Error>
    where
        F: FnMut(TargetBouquetStreamEventDto),
    {
        self.buffer.extend_from_slice(bytes);
        self.decode_available(on_event)?;
        if self.buffer.len() > MAX_STREAM_EVENT_BYTES {
            return Err(Error::DeserializeError);
        }
        Ok(())
    }

    fn finish<F>(&mut self, on_event: &mut F) -> Result<(), Error>
    where
        F: FnMut(TargetBouquetStreamEventDto),
    {
        self.decode_available(on_event)?;
        if self.started
            && self.finished
            && self.value_start.is_none()
            && self.buffer.iter().all(u8::is_ascii_whitespace)
        {
            Ok(())
        } else {
            Err(Error::DeserializeError)
        }
    }

    fn decode_available<F>(&mut self, on_event: &mut F) -> Result<(), Error>
    where
        F: FnMut(TargetBouquetStreamEventDto),
    {
        while self.cursor < self.buffer.len() {
            let byte = self.buffer[self.cursor];
            if !self.started {
                if byte.is_ascii_whitespace() {
                    self.cursor += 1;
                    continue;
                }
                if byte != b'[' {
                    return Err(Error::DeserializeError);
                }
                self.started = true;
                self.discard_through(self.cursor + 1);
                continue;
            }
            if self.finished {
                if !byte.is_ascii_whitespace() {
                    return Err(Error::DeserializeError);
                }
                self.cursor += 1;
                continue;
            }
            if self.value_start.is_none() {
                if byte.is_ascii_whitespace() || byte == b',' {
                    self.cursor += 1;
                    continue;
                }
                if byte == b']' {
                    self.finished = true;
                    self.discard_through(self.cursor + 1);
                    continue;
                }
                if byte != b'{' {
                    return Err(Error::DeserializeError);
                }
                self.value_start = Some(self.cursor);
            }

            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else {
                match byte {
                    b'"' => self.in_string = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth = self.depth.checked_sub(1).ok_or(Error::DeserializeError)?;
                        if self.depth == 0 {
                            let start = self.value_start.ok_or(Error::DeserializeError)?;
                            let end = self.cursor + 1;
                            if end - start > MAX_STREAM_EVENT_BYTES {
                                return Err(Error::DeserializeError);
                            }
                            let event = serde_json::from_slice::<TargetBouquetStreamEventDto>(&self.buffer[start..end])
                                .map_err(|_| Error::DeserializeError)?;
                            on_event(event);
                            self.value_start = None;
                            self.discard_through(end);
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            self.cursor += 1;
        }
        Ok(())
    }

    fn discard_through(&mut self, end: usize) {
        self.buffer.drain(..end);
        self.cursor = 0;
    }
}

#[derive(Default)]
pub struct TargetBouquetService;

impl TargetBouquetService {
    pub fn new() -> Self { Self }

    fn collection_url_from_base(base_href: &str) -> String {
        concat_path_leading_slash(base_href.trim_end_matches('/'), "api/v1/target-bouquets")
    }

    fn collection_url() -> String { Self::collection_url_from_base(&get_base_href()) }

    fn target_url(target_id: u16, suffix: Option<&str>) -> String {
        let collection = Self::collection_url();
        suffix.map_or_else(
            || concat_path_leading_slash(&collection, &target_id.to_string()),
            |suffix| concat_path_leading_slash(&collection, &format!("{target_id}/{suffix}")),
        )
    }

    pub async fn list_targets() -> Result<Vec<TargetBouquetTargetDto>, Error> {
        request_get::<Vec<TargetBouquetTargetDto>>(&Self::collection_url(), None, Some(Encoding::Json))
            .await?
            .ok_or(Error::NotFound)
    }

    /// Loads the selected target's bounded event stream and dispatches decoded events in wire order.
    pub async fn fetch_target_bouquet_stream<F>(
        target_id: u16,
        abort_signal: Option<web_sys::AbortSignal>,
        mut on_event: F,
    ) -> Result<(), Error>
    where
        F: FnMut(TargetBouquetStreamEventDto),
    {
        let url = Self::target_url(target_id, Some("groups"));
        let mut request = Request::get(&url).header("Accept", "application/json").abort_signal(abort_signal.as_ref());
        if let Some(token) = get_token() {
            request = request.header("Authorization", format!("Bearer {token}").as_str());
        }
        let response = request.send().await.map_err(|_| Error::RequestError)?;

        if !response.ok() {
            return Err(Error::HttpResponse(format!("HTTP {}", response.status())));
        }

        let content_type = response.headers().get("Content-Type").unwrap_or_default();
        if !content_type.starts_with("application/json") {
            return Err(Error::DeserializeError);
        }
        let body = response.body().ok_or(Error::DeserializeError)?;
        let reader = web_sys::ReadableStreamDefaultReader::new(&body).map_err(|_| Error::RequestError)?;
        let mut decoder = JsonArrayEventDecoder::default();
        loop {
            let result = JsFuture::from(reader.read()).await.map_err(|_| Error::RequestError)?;
            let done = Reflect::get(&result, &JsValue::from_str("done"))
                .map_err(|_| Error::RequestError)?
                .as_bool()
                .ok_or(Error::RequestError)?;
            if done {
                break;
            }
            let value = Reflect::get(&result, &JsValue::from_str("value")).map_err(|_| Error::RequestError)?;
            let chunk = Uint8Array::new(&value);
            let mut bytes = vec![0; chunk.length() as usize];
            chunk.copy_to(&mut bytes);
            decoder.push(&bytes, &mut on_event)?;
        }
        decoder.finish(&mut on_event)
    }

    pub async fn save_target_bouquet(target_id: u16, bouquet: &PlaylistClusterBouquetDto) -> Result<(), Error> {
        let url = Self::target_url(target_id, None);
        let _ = request_put::<_, serde_json::Value>(&url, bouquet, Some(Encoding::Json), Some(Encoding::Json)).await?;
        Ok(())
    }

    pub async fn delete_target_bouquet(target_id: u16) -> Result<(), Error> {
        let url = Self::target_url(target_id, None);
        let _ = request_delete::<serde_json::Value>(&url, None, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_url_never_becomes_protocol_relative() {
        assert_eq!(TargetBouquetService::collection_url_from_base("/"), "/api/v1/target-bouquets");
        assert_eq!(TargetBouquetService::collection_url_from_base("/tuliprox/"), "/tuliprox/api/v1/target-bouquets");
    }

    #[test]
    fn json_array_decoder_handles_split_nested_and_escaped_events() {
        let json = br#" [ {"type":"selection","groups":{"live":["News, \"HD\""],"vod":null,"series":null}},
            {"type":"input_chunk","input":"provider","cluster":"Live","groups":["Kids"],"is_last_for_cluster":true},
            {"type":"complete"} ] "#;
        let mut decoder = JsonArrayEventDecoder::default();
        let mut events = Vec::new();
        for chunk in json.chunks(7) {
            decoder.push(chunk, &mut |event| events.push(event)).unwrap();
        }
        decoder.finish(&mut |event| events.push(event)).unwrap();

        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], TargetBouquetStreamEventDto::Selection { .. }));
        assert!(matches!(events[1], TargetBouquetStreamEventDto::InputChunk { .. }));
        assert_eq!(events[2], TargetBouquetStreamEventDto::Complete);
    }

    #[test]
    fn json_array_decoder_rejects_oversized_incomplete_event() {
        let mut decoder = JsonArrayEventDecoder::default();
        let mut bytes = vec![b'a'; MAX_STREAM_EVENT_BYTES + 2];
        bytes[0] = b'[';
        bytes[1] = b'{';
        assert_eq!(decoder.push(&bytes, &mut |_| {}), Err(Error::DeserializeError));
    }
}
