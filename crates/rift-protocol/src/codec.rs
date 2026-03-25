//! Message framing codec for length-delimited messages

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

/// Encode a message with varint length prefix
pub fn encode_message(msg: &[u8], buf: &mut BytesMut) -> Result<(), CodecError> {
    if msg.len() > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(msg.len()));
    }

    // Encode length as varint
    encode_varint(msg.len() as u64, buf);

    // Write message
    buf.put_slice(msg);

    Ok(())
}

/// Decode a message from a buffer
pub fn decode_message(buf: &mut BytesMut) -> Result<Option<Vec<u8>>, CodecError> {
    // Try to read varint length
    let length = match decode_varint(buf)? {
        Some(len) => len as usize,
        None => return Ok(None), // Not enough data yet
    };

    if length > MAX_MESSAGE_SIZE {
        return Err(CodecError::MessageTooLarge(length));
    }

    // Check if we have enough data
    if buf.len() < length {
        return Ok(None);
    }

    // Read message
    let msg = buf.split_to(length).to_vec();
    Ok(Some(msg))
}

/// Encode a u64 as varint
fn encode_varint(mut value: u64, buf: &mut BytesMut) {
    while value >= 0x80 {
        buf.put_u8((value as u8) | 0x80);
        value >>= 7;
    }
    buf.put_u8(value as u8);
}

/// Decode a varint from buffer, consuming bytes if successful
fn decode_varint(buf: &mut BytesMut) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    let mut shift = 0;

    for i in 0..10 {
        // Max 10 bytes for u64
        if i >= buf.len() {
            return Ok(None); // Need more data
        }

        let byte = buf[i];
        value |= ((byte & 0x7F) as u64) << shift;

        if byte & 0x80 == 0 {
            // Last byte
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
        let msg = b"hello world";
        let mut buf = BytesMut::new();

        encode_message(msg, &mut buf).unwrap();
        let decoded = decode_message(&mut buf).unwrap().unwrap();

        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_empty() {
        let msg = b"";
        let mut buf = BytesMut::new();

        encode_message(msg, &mut buf).unwrap();
        let decoded = decode_message(&mut buf).unwrap().unwrap();

        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_decode_partial() {
        let msg = b"hello world";
        let mut buf = BytesMut::new();

        encode_message(msg, &mut buf).unwrap();

        // Split buffer - not enough data
        let mut partial = buf.split_to(5);
        assert!(decode_message(&mut partial).unwrap().is_none());
    }

    #[test]
    fn test_message_too_large() {
        let msg = vec![0u8; MAX_MESSAGE_SIZE + 1];
        let mut buf = BytesMut::new();

        let result = encode_message(&msg, &mut buf);
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_multiple_messages() {
        let mut buf = BytesMut::new();

        encode_message(b"first", &mut buf).unwrap();
        encode_message(b"second", &mut buf).unwrap();
        encode_message(b"third", &mut buf).unwrap();

        let msg1 = decode_message(&mut buf).unwrap().unwrap();
        let msg2 = decode_message(&mut buf).unwrap().unwrap();
        let msg3 = decode_message(&mut buf).unwrap().unwrap();

        assert_eq!(msg1, b"first");
        assert_eq!(msg2, b"second");
        assert_eq!(msg3, b"third");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_varint_encoding() {
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
}
