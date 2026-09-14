use crate::{
    protocol::{OriginStreamId, RunId},
    TestkitError,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::hash;

pub const MAGIC: [u8; 8] = *b"TPXTEST\0";
const VERSION: u8 = 1;
const FIXED_HEADER_LEN: usize = 8 + 1 + 1 + 2 + 16 + 16 + 4 + 8 + 8 + 4 + 4 + 4;
const FIXED_HEADER_LEN_U16: u16 = 76;
const TRAILER_LEN: usize = 4;
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub run_id: [u8; 16],
    pub origin_stream_id: [u8; 16],
    pub channel_marker: u32,
    pub sequence: u64,
    pub origin_mono_nanos: u64,
    pub configured_bps: u32,
    pub payload: Bytes,
}

impl Frame {
    #[must_use]
    pub fn synthetic(run_id: &RunId, stream_id: &OriginStreamId, marker: u32, sequence: u64) -> Self {
        let mut run = [0_u8; 16];
        let mut stream = [0_u8; 16];
        run.copy_from_slice(&blake3::hash(run_id.0.as_bytes()).as_bytes()[..16]);
        stream.copy_from_slice(&blake3::hash(stream_id.0.as_bytes()).as_bytes()[..16]);
        let payload = format!("marker={marker};sequence={sequence}\n").into_bytes();
        Self {
            run_id: run,
            origin_stream_id: stream,
            channel_marker: marker,
            sequence,
            origin_mono_nanos: 0,
            configured_bps: 64_000,
            payload: Bytes::from(payload),
        }
    }

    pub fn encode(&self, output: &mut BytesMut) -> Result<(), TestkitError> {
        let payload_len = self.payload.len();
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(TestkitError::Protocol("frame payload exceeds configured limit".to_owned()));
        }
        let start = output.len();
        output.reserve(FIXED_HEADER_LEN + payload_len + TRAILER_LEN);
        output.put_slice(&MAGIC);
        output.put_u8(VERSION);
        output.put_u8(0);
        output.put_u16(FIXED_HEADER_LEN_U16);
        output.put_slice(&self.run_id);
        output.put_slice(&self.origin_stream_id);
        output.put_u32(self.channel_marker);
        output.put_u64(self.sequence);
        output.put_u64(self.origin_mono_nanos);
        output.put_u32(self.configured_bps);
        output.put_u32(
            u32::try_from(payload_len)
                .map_err(|_| TestkitError::Protocol("frame payload length cannot be encoded".to_owned()))?,
        );
        let header_crc = hash(&output[start..]);
        output.put_u32(header_crc);
        output.put_slice(&self.payload);
        output.put_u32(hash(&self.payload));
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: BytesMut,
}

/// Verifies that decoded frames belong to one requested synthetic stream.
/// A late subscriber may start at any sequence, but it can never move backwards
/// or silently switch to another upstream response.
#[derive(Debug)]
pub struct FrameValidator {
    expected_run_id: [u8; 16],
    expected_marker: u32,
    origin_stream_id: Option<[u8; 16]>,
    last_sequence: Option<u64>,
}

impl FrameValidator {
    #[must_use]
    pub fn new(run_id: &RunId, expected_marker: u32) -> Self {
        let mut expected_run_id = [0_u8; 16];
        expected_run_id.copy_from_slice(&blake3::hash(run_id.0.as_bytes()).as_bytes()[..16]);
        Self { expected_run_id, expected_marker, origin_stream_id: None, last_sequence: None }
    }

    pub fn validate(&mut self, frame: &Frame) -> Result<(), TestkitError> {
        if frame.run_id != self.expected_run_id {
            return Err(TestkitError::Protocol("received a frame for a foreign test run".to_owned()));
        }
        if frame.channel_marker != self.expected_marker {
            return Err(TestkitError::Protocol(format!(
                "marker mismatch: expected {}, got {}",
                self.expected_marker, frame.channel_marker
            )));
        }
        if let Some(origin_stream_id) = self.origin_stream_id {
            if origin_stream_id != frame.origin_stream_id {
                return Err(TestkitError::Protocol("origin stream changed during a playback".to_owned()));
            }
        } else {
            self.origin_stream_id = Some(frame.origin_stream_id);
        }
        if let Some(previous) = self.last_sequence {
            if frame.sequence <= previous {
                return Err(TestkitError::Protocol(format!(
                    "non-increasing frame sequence: previous {previous}, current {}",
                    frame.sequence
                )));
            }
        }
        let expected_payload = format!("marker={};sequence={}\n", frame.channel_marker, frame.sequence);
        if frame.payload.as_ref() != expected_payload.as_bytes() {
            return Err(TestkitError::Protocol(
                "synthetic frame payload does not match marker and sequence".to_owned(),
            ));
        }
        self.last_sequence = Some(frame.sequence);
        Ok(())
    }
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TestkitError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let Some(position) = self.buffer.windows(MAGIC.len()).position(|window| window == MAGIC) else {
                self.keep_magic_suffix();
                return Ok(frames);
            };
            if position != 0 {
                self.buffer.advance(position);
            }
            if self.buffer.len() < FIXED_HEADER_LEN {
                return Ok(frames);
            }
            let header_len = u16::from_be_bytes([self.buffer[10], self.buffer[11]]) as usize;
            if header_len != FIXED_HEADER_LEN || self.buffer[8] != VERSION {
                self.buffer.advance(1);
                continue;
            }
            let expected_header_crc = u32::from_be_bytes(
                self.buffer[FIXED_HEADER_LEN - 4..FIXED_HEADER_LEN]
                    .try_into()
                    .map_err(|_| TestkitError::Protocol("invalid header length".to_owned()))?,
            );
            if hash(&self.buffer[..FIXED_HEADER_LEN - 4]) != expected_header_crc {
                self.buffer.advance(1);
                continue;
            }
            let payload_len = u32::from_be_bytes(
                self.buffer[68..72]
                    .try_into()
                    .map_err(|_| TestkitError::Protocol("invalid payload length".to_owned()))?,
            ) as usize;
            if payload_len > MAX_PAYLOAD_LEN {
                self.buffer.advance(1);
                continue;
            }
            let total_len = FIXED_HEADER_LEN + payload_len + TRAILER_LEN;
            if self.buffer.len() < total_len {
                return Ok(frames);
            }
            let payload_start = FIXED_HEADER_LEN;
            let payload_end = payload_start + payload_len;
            let expected_payload_crc = u32::from_be_bytes(
                self.buffer[payload_end..total_len]
                    .try_into()
                    .map_err(|_| TestkitError::Protocol("invalid trailer length".to_owned()))?,
            );
            if hash(&self.buffer[payload_start..payload_end]) != expected_payload_crc {
                self.buffer.advance(1);
                continue;
            }
            let mut run_id = [0_u8; 16];
            let mut origin_stream_id = [0_u8; 16];
            run_id.copy_from_slice(&self.buffer[12..28]);
            origin_stream_id.copy_from_slice(&self.buffer[28..44]);
            let frame = Frame {
                run_id,
                origin_stream_id,
                channel_marker: u32::from_be_bytes(
                    self.buffer[44..48].try_into().map_err(|_| TestkitError::Protocol("invalid marker".to_owned()))?,
                ),
                sequence: u64::from_be_bytes(
                    self.buffer[48..56]
                        .try_into()
                        .map_err(|_| TestkitError::Protocol("invalid sequence".to_owned()))?,
                ),
                origin_mono_nanos: u64::from_be_bytes(
                    self.buffer[56..64]
                        .try_into()
                        .map_err(|_| TestkitError::Protocol("invalid timestamp".to_owned()))?,
                ),
                configured_bps: u32::from_be_bytes(
                    self.buffer[64..68].try_into().map_err(|_| TestkitError::Protocol("invalid bitrate".to_owned()))?,
                ),
                payload: self.buffer[payload_start..payload_end].to_vec().into(),
            };
            self.buffer.advance(total_len);
            frames.push(frame);
        }
    }

    fn keep_magic_suffix(&mut self) {
        let retained = self.buffer.len().min(MAGIC.len() - 1);
        let start = self.buffer.len() - retained;
        let suffix = self.buffer.split_off(start);
        self.buffer = suffix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_every_network_split() {
        let frame = Frame::synthetic(&RunId::new("run"), &OriginStreamId::new("origin"), 17, 0);
        let mut encoded = BytesMut::new();
        frame.encode(&mut encoded).unwrap();
        for split in 0..=encoded.len() {
            let mut decoder = FrameDecoder::default();
            let mut frames = decoder.push(&encoded[..split]).unwrap();
            frames.extend(decoder.push(&encoded[split..]).unwrap());
            assert_eq!(frames, vec![frame.clone()]);
        }
    }

    #[test]
    fn validator_rejects_foreign_runs_and_sequence_replay() {
        let run = RunId::new("run");
        let stream = OriginStreamId::new("origin");
        let mut validator = FrameValidator::new(&run, 17);
        assert!(validator.validate(&Frame::synthetic(&run, &stream, 17, 4)).is_ok());
        assert!(validator.validate(&Frame::synthetic(&run, &stream, 17, 4)).is_err());
        assert!(validator.validate(&Frame::synthetic(&RunId::new("other"), &stream, 17, 5)).is_err());
    }
}
