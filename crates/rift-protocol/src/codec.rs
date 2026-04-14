//! Message framing codec for type-and-length-delimited messages
//!
//! Wire format: varint(type_id) || varint(length) || payload
//!
//! The type byte tells the receiver which protobuf message to decode (or that
//! the payload is raw bytes for BLOCK_DATA frames at 0xF0+).

use bytes::{Buf, BufMut, BytesMut};
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

/// Encode a message with varint type_id prefix and varint length prefix.
pub fn encode_message(type_id: u8, payload: &[u8], buf: &mut BytesMut) -> Result<(), CodecError> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(payload.len()));
    }

    encode_varint(type_id as u64, buf);
    encode_varint(payload.len() as u64, buf);
    buf.put_slice(payload);

    Ok(())
}

/// Decode a message from a buffer.
///
/// Returns `Ok(Some((type_id, payload)))` when a complete frame is available,
/// `Ok(None)` when more data is needed, or an error on malformed input.
pub fn decode_message(buf: &mut BytesMut) -> Result<Option<(u8, Vec<u8>)>, CodecError> {
    // Peek at the buffer without consuming to check if we have enough data.
    let mut peek = &buf[..];

    let type_id = match decode_varint_peek(&mut peek)? {
        Some(v) => v,
        None => return Ok(None),
    };

    let length = match decode_varint_peek(&mut peek)? {
        Some(v) => v as usize,
        None => return Ok(None),
    };

    if length > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(length));
    }

    if peek.len() < length {
        return Ok(None);
    }

    // We have a complete frame — now consume from the real buffer.
    decode_varint(buf)?.unwrap(); // type_id
    decode_varint(buf)?.unwrap(); // length
    let payload = buf.split_to(length).to_vec();

    Ok(Some((type_id as u8, payload)))
}

/// Encode a u64 as varint.
fn encode_varint(mut value: u64, buf: &mut BytesMut) {
    while value >= 0x80 {
        buf.put_u8((value as u8) | 0x80);
        value >>= 7;
    }
    buf.put_u8(value as u8);
}

/// Decode a varint from a mutable byte slice without a BytesMut (peek, no consume).
fn decode_varint_peek(buf: &mut &[u8]) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    let mut shift = 0;

    for i in 0..10 {
        if i >= buf.len() {
            return Ok(None);
        }
        let byte = buf[i];
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            *buf = &buf[i + 1..];
            return Ok(Some(value));
        }
        shift += 7;
    }

    Err(CodecError::InvalidVarint)
}

/// Decode a varint from a BytesMut, consuming bytes.
fn decode_varint(buf: &mut BytesMut) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    let mut shift = 0;

    for i in 0..10 {
        if i >= buf.len() {
            return Ok(None);
        }
        let byte = buf[i];
        value |= ((byte & 0x7F) as u64) << shift;
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

    #[test]
    fn test_encode_decode_message() {
        let mut buf = BytesMut::new();
        encode_message(0x01, b"hello world", &mut buf).unwrap();
        let (type_id, payload) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(payload, b"hello world");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_encode_decode_empty_payload() {
        let mut buf = BytesMut::new();
        encode_message(0x02, b"", &mut buf).unwrap();
        let (type_id, payload) = decode_message(&mut buf).unwrap().unwrap();
        assert_eq!(type_id, 0x02);
        assert_eq!(payload, b"");
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

        assert_eq!((t1, p1.as_slice()), (0x01, b"first" as &[u8]));
        assert_eq!((t2, p2.as_slice()), (0x30, b"second" as &[u8]));
        assert_eq!((t3, p3.as_slice()), (0xF0, b"third" as &[u8]));
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
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_codec_round_trip(type_id: u8, payload: Vec<u8>) {
            prop_assume!(payload.len() <= MAX_MESSAGE_SIZE);

            let mut buf = BytesMut::new();
            encode_message(type_id, &payload, &mut buf).unwrap();
            let (decoded_type, decoded_payload) = decode_message(&mut buf).unwrap().unwrap();

            prop_assert_eq!(decoded_type, type_id);
            prop_assert_eq!(decoded_payload, payload);
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

            let expected: Vec<(u8, Vec<u8>)> = messages;
            prop_assert_eq!(decoded, expected);
        }
    }
}
