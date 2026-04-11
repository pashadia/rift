//! Protocol-level handshake helpers.
//!
//! These are thin send/receive wrappers over [`RiftStream`] — they marshal
//! and unmarshal the `RiftHello` / `RiftWelcome` protobuf messages using the
//! varint-framed codec, but contain no share-validation logic.
//!
//! **Server usage:**
//! ```ignore
//! let mut ctrl = conn.accept_stream().await?;
//! let hello  = recv_hello(&mut ctrl).await?;
//! // validate hello.share_name, build welcome ...
//! send_welcome(&mut ctrl, welcome).await?;
//! ```
//!
//! **Client usage:**
//! ```ignore
//! let mut ctrl = conn.open_stream().await?;
//! let welcome = client_handshake(&mut ctrl, "my-share", &[]).await?;
//! ```

use prost::Message as _;
use tracing::instrument;

use rift_protocol::messages::msg;
pub use rift_protocol::messages::{RiftHello, RiftWelcome};

use crate::connection::RiftStream;
use crate::TransportError;

/// Current Rift wire-protocol version.
pub const RIFT_PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Send a `RiftHello` and receive a `RiftWelcome` on a single stream.
///
/// This is the complete client-side handshake in one call.
/// `capabilities` is the list of optional capability enum values (as `i32`)
/// the client wishes to negotiate; pass `&[]` for none.
#[instrument(skip(stream), fields(share_name = %share_name, capabilities_len = capabilities.len()), err)]
pub async fn client_handshake<S: RiftStream>(
    stream: &mut S,
    share_name: &str,
    capabilities: &[i32],
) -> Result<RiftWelcome, TransportError> {
    let hello = RiftHello {
        protocol_version: RIFT_PROTOCOL_VERSION,
        capabilities: capabilities.to_vec(),
        share_name: share_name.to_string(),
    };
    stream
        .send_frame(msg::RIFT_HELLO, &hello.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    // Wait for the server's welcome frame.
    loop {
        match stream.recv_frame().await? {
            None => {
                return Err(TransportError::Codec(rift_protocol::codec::CodecError::Io(
                    std::io::Error::other("stream closed before RiftWelcome received"),
                )))
            }
            Some((type_id, payload)) if type_id == msg::RIFT_WELCOME => {
                return RiftWelcome::decode(payload.as_ref()).map_err(|e| {
                    TransportError::Codec(rift_protocol::codec::CodecError::Io(
                        std::io::Error::other(format!("RiftWelcome decode error: {e}")),
                    ))
                });
            }
            Some(_) => continue, // skip unexpected frames
        }
    }
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// Receive and decode the client's `RiftHello` from a stream.
///
/// Returns `Err` if the first frame is not `RIFT_HELLO` or the payload cannot
/// be decoded.
#[instrument(skip(stream), err)]
pub async fn recv_hello<S: RiftStream>(stream: &mut S) -> Result<RiftHello, TransportError> {
    match stream.recv_frame().await? {
        None => Err(TransportError::Codec(rift_protocol::codec::CodecError::Io(
            std::io::Error::other("stream closed before RiftHello received"),
        ))),
        Some((type_id, _)) if type_id != msg::RIFT_HELLO => Err(TransportError::Codec(
            rift_protocol::codec::CodecError::Io(std::io::Error::other(format!(
                "expected RIFT_HELLO (0x{:02X}), got 0x{:02X}",
                msg::RIFT_HELLO,
                type_id,
            ))),
        )),
        Some((_, payload)) => RiftHello::decode(payload.as_ref()).map_err(|e| {
            TransportError::Codec(rift_protocol::codec::CodecError::Io(std::io::Error::other(
                format!("RiftHello decode error: {e}"),
            )))
        }),
    }
}

/// Encode and send a `RiftWelcome` on the given stream, then half-close.
///
/// Called by the server after it has validated the `RiftHello` and decided
/// what capabilities and root handle to offer.
#[instrument(skip(stream), fields(root_handle_len = welcome.root_handle.len()), err)]
pub async fn send_welcome<S: RiftStream>(
    stream: &mut S,
    welcome: RiftWelcome,
) -> Result<(), TransportError> {
    stream
        .send_frame(msg::RIFT_WELCOME, &welcome.encode_to_vec())
        .await?;
    stream.finish_send().await?;
    Ok(())
}
