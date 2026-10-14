//! MPEG-TS parsing and buffering.
//!
//! A prepared transport-stream byte buffer and the probe that reads structure
//! out of one. The two are mutually dependent - the buffer asks the probe for a
//! track signature, and the probe reports timestamp profiles the buffer defines -
//! so they form one cohesive unit rather than two layers.
//!
//! This lives below both `model` and `api` because both need it: configured
//! custom-stream responses hold prepared buffers, and the streaming and HLS
//! layers parse live ones. Nothing here may name `api`, `AppState` or a
//! repository.

pub mod transport_stream_buffer;
pub mod ts_inspector;

pub const TS_PACKET_SIZE: usize = 188;
pub const TS_SYNC_BYTE: u8 = 0x47;

/// Finds the first offset `o < 188` where `0x47` appears at `o`, `o + 188`, `o + 376`.
/// With fewer than 564 bytes at EOF, all available positions must match.
#[must_use]
pub fn ts_sync_offset(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    let limit = TS_PACKET_SIZE.min(buf.len());
    'offset_loop: for o in 0..limit {
        let mut pos = o;
        while pos < buf.len() && pos <= o + 2 * TS_PACKET_SIZE {
            if buf[pos] != TS_SYNC_BYTE {
                continue 'offset_loop;
            }
            pos += TS_PACKET_SIZE;
        }
        return Some(o);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_sync_offset_empty() {
        assert_eq!(ts_sync_offset(&[]), None);
    }

    #[test]
    fn ts_sync_offset_aligned() {
        let mut buf = vec![0u8; 564];
        buf[0] = 0x47;
        buf[188] = 0x47;
        buf[376] = 0x47;
        assert_eq!(ts_sync_offset(&buf), Some(0));
    }

    #[test]
    fn ts_sync_offset_with_garbage_prefix() {
        let mut buf = vec![0u8; 17];
        let mut ts = vec![0u8; 564];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        buf.extend(ts);
        assert_eq!(ts_sync_offset(&buf), Some(17));
    }

    #[test]
    fn ts_sync_offset_short_buffer_at_eof() {
        let mut buf = vec![0u8; 200];
        buf[10] = 0x47;
        buf[198] = 0x47;
        assert_eq!(ts_sync_offset(&buf), Some(10));
    }

    #[test]
    fn ts_sync_offset_corrupt_packet() {
        let mut buf = vec![0u8; 564];
        buf[0] = 0x47;
        buf[188] = 0x48; // corrupt
        buf[376] = 0x47;
        assert_eq!(ts_sync_offset(&buf), None);
    }
}
