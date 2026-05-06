//! Message framing codec for type-and-length-delimited messages
//!
//! Wire format: `varint(type_id)` || varint(length) || payload
//!
//! The type byte tells the receiver which protobuf message to decode (or that
//! the payload is raw bytes for `BLOCK_DATA` frames at 0xF0+).

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CodecError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Message too large: {0} bytes")]
    MessageTooLarge(usize),

    #[error("Invalid varint")]
    InvalidVarint,
}

/// Maximum message size (16 MB)
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Encode a message with varint `type_id` prefix and varint length prefix.
pub fn encode_message(type_id: u8, payload: &[u8], buf: &mut BytesMut) -> Result<(), CodecError> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(payload.len()));
    }

    encode_header(type_id, payload.len(), buf)?;
    buf.put_slice(payload);

    Ok(())
}

/// Encode only the frame header (varint `type_id` + varint `payload_len`) into `buf`.
///
/// This is useful when the caller wants to write the header and payload
/// separately, avoiding a per-frame copy of the payload into an intermediate
/// buffer.
pub fn encode_header(
    type_id: u8,
    payload_len: usize,
    buf: &mut impl BufMut,
) -> Result<(), CodecError> {
    if payload_len > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(payload_len));
    }

    encode_varint(type_id.into(), buf);
    encode_varint(payload_len as u64, buf);

    Ok(())
}

/// Peek at a frame header (`type_id` and `payload_len`) without consuming the buffer.
///
/// Returns `Ok(Some((type_id, payload_len, header_len)))` when a complete header
/// is available, `Ok(None)` when more data is needed, or an error on malformed input.
/// `header_len` is the number of bytes consumed by the two varints.
pub fn try_decode_header(buf: &[u8]) -> Result<Option<(u8, usize, usize)>, CodecError> {
    let mut peek = buf;

    let Some(type_id) = decode_varint_peek(&mut peek)? else {
        return Ok(None);
    };

    let Some(payload_len) = decode_varint_peek(&mut peek)? else {
        return Ok(None);
    };

    let payload_len = usize::try_from(payload_len).expect("payload_len fits in usize");
    let header_len = buf.len() - peek.len();

    Ok(Some((
        u8::try_from(type_id).expect("type_id is 0x00-0xFF"),
        payload_len,
        header_len,
    )))
}

/// Decode a message from a buffer.
///
/// Returns `Ok(Some((type_id, payload)))` when a complete frame is available,
/// `Ok(None)` when more data is needed, or an error on malformed input.
pub fn decode_message(buf: &mut BytesMut) -> Result<Option<(u8, Bytes)>, CodecError> {
    // Peek at the buffer without consuming to check if we have enough data.
    let mut peek = &buf[..];

    let Some(type_id) = decode_varint_peek(&mut peek)? else {
        return Ok(None);
    };

    let length = match decode_varint_peek(&mut peek)? {
        Some(v) => usize::try_from(v).expect("frame length fits in usize"),
        None => return Ok(None),
    };

    if length > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(length));
    }

    if peek.len() < length {
        return Ok(None);
    }

    // We have a complete frame — now consume from the real buffer.
    decode_varint(buf)?.ok_or(CodecError::InvalidVarint)?; // type_id
    decode_varint(buf)?.ok_or(CodecError::InvalidVarint)?; // length
    let payload = buf.split_to(length).freeze();

    Ok(Some((
        u8::try_from(type_id).expect("type_id is 0x00-0xFF"),
        payload,
    )))
}

/// Encode a u64 as varint.
fn encode_varint(mut value: u64, buf: &mut impl BufMut) {
    while value >= 0x80 {
        buf.put_u8(u8::try_from(value & 0x7F).expect("value & 0x7F fits in u8") | 0x80);
        value >>= 7;
    }
    buf.put_u8(u8::try_from(value).expect("varint final byte fits in u8"));
}

/// Decode a varint from a mutable byte slice without a `BytesMut` (peek, no consume).
fn decode_varint_peek(buf: &mut &[u8]) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    let mut shift = 0;

    for i in 0..10 {
        if i >= buf.len() {
            return Ok(None);
        }
        let byte = buf[i];
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            *buf = &buf[i + 1..];
            return Ok(Some(value));
        }
        shift += 7;
    }

    Err(CodecError::InvalidVarint)
}

/// Decode a varint from a `BytesMut`, consuming bytes.
fn decode_varint(buf: &mut BytesMut) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    let mut shift = 0;

    for i in 0..10 {
        if i >= buf.len() {
            return Ok(None);
        }
        let byte = buf[i];
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            buf.advance(i + 1);
            return Ok(Some(value));
        }
        shift += 7;
    }

    Err(CodecError::InvalidVarint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_encode_decode_message() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello world", &mut buf).unwrap();
        let (type_id, payload) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(&payload[..], b"hello world");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_encode_decode_empty_payload() {
        let mut buf = BytesMut::new();
        encode_message(0x02, b"", &mut buf).unwrap();
        let (type_id, payload) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0x02);
        assert_eq!(&payload[..], b"");
    }

    #[test]
    fn test_type_id_preserved() {
        let type_ids: &[u8] = &[0x01, 0x0F, 0x10, 0x30, 0x50, 0x60, 0xF0, 0x7F];
        for &id in type_ids {
            let mut buf = BytesMut::new();
            encode_message(id, b"data", &mut buf).unwrap();
            let (decoded_id, _) = decode_message(&mut buf).unwrap().unwrap();
            assert_eq!(decoded_id, id, "type_id 0x{id:02X} not preserved");
        }
    }

    #[test]
    fn test_decode_partial_header() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello world", &mut buf).unwrap();
        // Only provide the first byte — not enough for a complete frame
        let mut partial = buf.split_to(1);
        assert!(decode_message(&mut partial).unwrap().is_none());
    }

    #[test]
    fn test_decode_partial_payload() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello world", &mut buf).unwrap();
        // Drop the last byte so the payload is incomplete
        let len = buf.len();
        let mut partial = buf.split_to(len - 1);
        assert!(decode_message(&mut partial).unwrap().is_none());
    }

    #[test]
    fn test_message_too_large() {
        let payload = vec![0u8; MAX_MESSAGE_SIZE + 1];
        let mut buf = BytesMut::new();
        let result = encode_message(0x01, &payload, &mut buf);
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_message_size_at_max_allowed() {
        let payload = vec![0u8; MAX_MESSAGE_SIZE];
        let mut buf = BytesMut::new();
        encode_message(0x01, &payload, &mut buf).unwrap();
        let (type_id, decoded) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(decoded.len(), MAX_MESSAGE_SIZE);
    }

    #[test]
    fn test_decode_oversized_length() {
        let mut buf = BytesMut::with_capacity(64);
        buf.put_u8(0x01);
        encode_varint((MAX_MESSAGE_SIZE + 1) as u64, &mut buf);
        let result = decode_message(&mut buf);
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_decode_oversized_length_at_limit() {
        let mut buf = BytesMut::with_capacity(64);
        buf.put_u8(0x01);
        encode_varint(MAX_MESSAGE_SIZE as u64, &mut buf);
        let result = decode_message(&mut buf);
        assert!(result.is_ok(), "exactly at limit should be ok");
    }

    #[test]
    fn test_multiple_messages_in_sequence() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"first", &mut buf).unwrap();
        encode_message(0x30, b"second", &mut buf).unwrap();
        encode_message(0xF0, b"third", &mut buf).unwrap();

        let (t1, p1) = decode_message(&mut buf).unwrap().unwrap();
        let (t2, p2) = decode_message(&mut buf).unwrap().unwrap();
        let (t3, p3) = decode_message(&mut buf).unwrap().unwrap();

        assert_eq!((t1, &p1[..]), (0x01, b"first" as &[u8]));
        assert_eq!((t2, &p2[..]), (0x30, b"second" as &[u8]));
        assert_eq!((t3, &p3[..]), (0xF0, b"third" as &[u8]));
        assert!(buf.is_empty());
    }

    #[test]
    fn test_block_data_raw_bytes() {
        // 0xF0 is BLOCK_DATA — raw bytes, not protobuf, but same framing
        let chunk = vec![0xAB_u8; 131_072]; // 128 KB
        let mut buf = BytesMut::new();
        encode_message(0xF0, &chunk, &mut buf).unwrap();
        let (type_id, payload) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0xF0);
        assert_eq!(payload, chunk);
    }

    #[test]
    fn test_varint_encoding_sizes() {
        let mut buf = BytesMut::new();

        encode_varint(0, &mut buf);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_varint(127, &mut buf);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_varint(128, &mut buf);
        assert_eq!(buf.len(), 2);

        buf.clear();
        encode_varint(u64::MAX, &mut buf);
        assert_eq!(buf.len(), 10);
    }

    #[test]
    fn test_varint_round_trip() {
        let values = [0, 1, 127, 128, 255, 256, 65535, 65536, u64::MAX];
        for &value in &values {
            let mut buf = BytesMut::new();
            encode_varint(value, &mut buf);
            let decoded = decode_varint(&mut buf).unwrap().unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn test_encode_header_matches_encode_message_prefix() {
        // encode_header should produce the same bytes as the header portion
        // (varint type + varint length) of encode_message.
        let payload = b"hello world";

        // Full encoding for reference.
        let mut full = BytesMut::new();
        encode_message(0x01, payload, &mut full).unwrap();
        let expected_header_len = full.len() - payload.len();
        let expected_header = &full[..expected_header_len];

        // Header-only encoding.
        let mut header_only = BytesMut::new();
        encode_header(0x01, payload.len(), &mut header_only).unwrap();
        assert_eq!(&header_only[..], expected_header);
    }

    #[test]
    fn test_encode_header_empty_payload() {
        let mut full = BytesMut::new();
        encode_message(0xFF, b"", &mut full).unwrap();
        let expected_header = &full[..];

        let mut header_only = BytesMut::new();
        encode_header(0xFF, 0, &mut header_only).unwrap();
        assert_eq!(&header_only[..], expected_header);
    }

    #[test]
    fn test_encode_header_large_type_id() {
        // type_id above 127 requires 2-byte varint
        let payload = b"test";
        let mut full = BytesMut::new();
        encode_message(0x80, payload, &mut full).unwrap();
        let expected_header_len = full.len() - payload.len();
        let expected_header = &full[..expected_header_len];

        let mut header_only = BytesMut::new();
        encode_header(0x80, payload.len(), &mut header_only).unwrap();
        assert_eq!(&header_only[..], expected_header);
    }

    #[test]
    fn test_encode_header_large_payload() {
        // payload length 300 requires 2-byte varint
        let payload = vec![0u8; 300];
        let mut full = BytesMut::new();
        encode_message(0x01, &payload, &mut full).unwrap();
        let expected_header_len = full.len() - payload.len();
        let expected_header = &full[..expected_header_len];

        let mut header_only = BytesMut::new();
        encode_header(0x01, payload.len(), &mut header_only).unwrap();
        assert_eq!(&header_only[..], expected_header);
    }

    #[test]
    fn test_encode_header_too_large() {
        let result = encode_header(0x01, MAX_MESSAGE_SIZE + 1, &mut BytesMut::new());
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_varint_boundary_at_128() {
        let mut buf = BytesMut::new();
        encode_varint(127, &mut buf);
        assert_eq!(buf.len(), 1, "127 should be single byte");

        buf.clear();
        encode_varint(128, &mut buf);
        assert_eq!(buf.len(), 2, "128 should need two bytes (continuation bit)");
    }

    #[test]
    fn test_wire_format_type_before_length() {
        // Verify the on-wire order: varint(type) || varint(length) || payload
        // For type=0x01, payload=b"hi" (2 bytes):
        //   type:   0x01 (1 byte varint)
        //   length: 0x02 (1 byte varint)
        //   payload: b"hi"
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hi", &mut buf).unwrap();
        assert_eq!(&buf[..], &[0x01, 0x02, b'h', b'i']);
    }

    #[test]
    fn test_decode_message_returns_bytes_not_vec() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello", &mut buf).unwrap();
        let result = decode_message(&mut buf).unwrap().unwrap();
        // Verify the payload is Bytes (zero-copy from buffer)
        let (_type_id, payload): (u8, Bytes) = result;
        assert_eq!(&payload[..], b"hello");
    }

    // ── try_decode_header tests ──────────────────────────────────────────

    #[test]
    fn test_try_decode_header_valid() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello world", &mut buf).unwrap();

        let (type_id, payload_len, header_len) = try_decode_header(&buf).unwrap().unwrap();

        assert_eq!(type_id, 0x01);
        assert_eq!(payload_len, 11);
        assert_eq!(header_len, 2); // 1-byte type + 1-byte len
        assert_eq!(buf.len(), header_len + payload_len);
    }

    #[test]
    fn test_try_decode_header_partial() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"data", &mut buf).unwrap();

        // Only first byte — not a complete header
        let partial = &buf[..1];
        assert!(try_decode_header(partial).unwrap().is_none());

        // Just enough for complete header
        let header_slice = &buf[..2];
        let (type_id, payload_len, header_len) = try_decode_header(header_slice).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(payload_len, 4);
        assert_eq!(header_len, 2);
    }

    #[test]
    fn test_try_decode_header_empty_input() {
        assert!(try_decode_header(&[]).unwrap().is_none());
    }

    #[test]
    fn test_try_decode_header_invalid_varint() {
        // 10 bytes all with continuation bit set → invalid varint
        let buf: Vec<u8> = vec![0x80; 12];
        let result = try_decode_header(&buf);
        assert!(matches!(result, Err(CodecError::InvalidVarint)));
    }

    #[test]
    fn test_try_decode_header_empty_payload() {
        let mut buf = BytesMut::new();
        encode_message(0x02, b"", &mut buf).unwrap();

        let (type_id, payload_len, header_len) = try_decode_header(&buf).unwrap().unwrap();
        assert_eq!(type_id, 0x02);
        assert_eq!(payload_len, 0);
        assert_eq!(header_len, 2);
    }

    #[test]
    fn test_try_decode_header_large_type_id() {
        let mut buf = BytesMut::new();
        // type_id 0x80 needs 2-byte varint
        encode_message(0x80, b"test", &mut buf).unwrap();

        let (type_id, payload_len, header_len) = try_decode_header(&buf).unwrap().unwrap();
        assert_eq!(type_id, 0x80);
        assert_eq!(payload_len, 4);
        assert_eq!(header_len, 3); // 2-byte type + 1-byte len
    }

    #[test]
    fn test_try_decode_header_large_payload_len() {
        let mut buf = BytesMut::new();
        // payload length 300 needs 2-byte varint
        let data = vec![0u8; 300];
        encode_message(0x01, &data, &mut buf).unwrap();

        let (type_id, payload_len, header_len) = try_decode_header(&buf).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(payload_len, 300);
        assert_eq!(header_len, 3); // 1-byte type + 2-byte len
    }

    #[test]
    fn test_try_decode_header_multiple_headers() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"first", &mut buf).unwrap();
        encode_message(0x30, b"second", &mut buf).unwrap();

        // First header
        let (t1, l1, h1) = try_decode_header(&buf).unwrap().unwrap();
        assert_eq!(t1, 0x01);
        assert_eq!(l1, 5);
        assert_eq!(h1, 2);

        // Slice past first frame
        let rest = &buf[h1 + l1..];
        let (t2, l2, h2) = try_decode_header(rest).unwrap().unwrap();
        assert_eq!(t2, 0x30);
        assert_eq!(l2, 6);
        assert_eq!(h2, 2);
    }

    #[test]
    fn test_try_decode_header_partial_second_varint() {
        let mut buf = BytesMut::new();
        // type_id 0x80 needs 2 bytes, so after reading the first varint
        // we might have only part of the second
        encode_message(0x80, &vec![0u8; 300], &mut buf).unwrap();
        // Header: 2-byte type + 2-byte len = 4 bytes
        assert_eq!(buf[0], 0x80); // first byte: type_id continuation
        assert_eq!(buf[1], 0x01); // second byte: type_id final

        // Give only 3 bytes: enough for type varint but not length varint
        let partial = &buf[..3];
        assert!(try_decode_header(partial).unwrap().is_none());
    }

    #[test]
    fn test_try_decode_header_does_not_consume_buffer() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"immutable", &mut buf).unwrap();

        let original_len = buf.len();
        let _ = try_decode_header(&buf).unwrap().unwrap();

        // Buffer length unchanged after peek
        assert_eq!(buf.len(), original_len);
    }

    #[test]
    fn test_try_decode_header_all_type_ids() {
        let type_ids: &[u8] = &[0x00, 0x01, 0x7F, 0x80, 0xF0, 0xFF];
        for &id in type_ids {
            let mut buf = BytesMut::new();
            encode_message(id, b"test", &mut buf).unwrap();

            let (decoded_id, payload_len, header_len) = try_decode_header(&buf).unwrap().unwrap();
            assert_eq!(decoded_id, id, "type_id 0x{id:02X} not preserved");
            assert_eq!(payload_len, 4);
            // header_len: 1 for types < 0x80, 2 for >= 0x80 (plus 1 for length)
            let expected = if id < 0x80 { 2 } else { 3 };
            assert_eq!(header_len, expected, "header_len for 0x{id:02X}");
        }
    }

    #[test]
    fn test_try_decode_header_max_message_size() {
        let mut buf = BytesMut::new();
        encode_message(0x01, &vec![0u8; MAX_MESSAGE_SIZE], &mut buf).unwrap();

        let (type_id, payload_len, _header_len) = try_decode_header(&buf).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(payload_len, MAX_MESSAGE_SIZE);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use bytes::Bytes;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_codec_round_trip(type_id: u8, payload: Vec<u8>) {
            prop_assume!(payload.len() <= MAX_MESSAGE_SIZE);

            let mut buf = BytesMut::new();
            encode_message(type_id, &payload, &mut buf).unwrap();
            let (decoded_type, decoded_payload) = decode_message(&mut buf).unwrap().unwrap();

            prop_assert_eq!(decoded_type, type_id);
            prop_assert_eq!(&decoded_payload[..], &payload[..]);
        }

        #[test]
        fn prop_codec_multiple_messages(messages: Vec<(u8, Vec<u8>)>) {
            let messages: Vec<_> = messages.into_iter()
                .filter(|(_, p)| p.len() <= MAX_MESSAGE_SIZE)
                .collect();

            let mut buf = BytesMut::new();
            for (type_id, payload) in &messages {
                encode_message(*type_id, payload, &mut buf).unwrap();
            }

            let mut decoded = Vec::new();
            while let Some(frame) = decode_message(&mut buf).unwrap() {
                decoded.push(frame);
            }

            let expected: Vec<(u8, Bytes)> = messages.into_iter().map(|(t, p)| (t, Bytes::from(p))).collect();
            prop_assert_eq!(decoded, expected);
        }
    }
}
