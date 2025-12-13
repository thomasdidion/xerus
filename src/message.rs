//! # BitTorrent Peer Wire Messages
//!
//! This module defines the message types and serialization for the BitTorrent
//! peer wire protocol. All communication between peers uses these messages.
//!
//! ## Message Structure
//!
//! Every message follows the same format:
//!
//! ```text
//! <length prefix><message ID><payload>
//! ```
//!
//! - **Length Prefix**: 4 bytes (big-endian u32) - Total length of message ID + payload
//! - **Message ID**: 1 byte - Identifies the message type
//! - **Payload**: Variable length - Message-specific data
//!
//! ## Message Types
//!
//! | ID | Name | Description |
//! |----|------|-------------|
//! | 0 | CHOKE | Peer will not send pieces (no payload) |
//! | 1 | UNCHOKE | Peer will send pieces (no payload) |
//! | 2 | INTERESTED | Client wants to download (no payload) |
//! | 3 | NOT INTERESTED | Client doesn't want to download (no payload) |
//! | 4 | HAVE | Peer has a piece (payload: piece index) |
//! | 5 | BITFIELD | Peer's piece availability (payload: bitfield) |
//! | 6 | REQUEST | Request a block (payload: index, begin, length) |
//! | 7 | PIECE | Block data (payload: index, begin, data) |
//! | 8 | CANCEL | Cancel a request (payload: index, begin, length) |
//!
//! ## Keep-Alive Messages
//!
//! A keep-alive message has length 0 and no ID or payload. It's sent periodically
//! to prevent connection timeouts.

use anyhow::Result;
use byteorder::{BigEndian, WriteBytesExt};

type MessageId = u8;
type MessagePayload = Vec<u8>;

pub const MESSAGE_CHOKE: MessageId = 0;
pub const MESSAGE_UNCHOKE: MessageId = 1;
pub const MESSAGE_INTERESTED: MessageId = 2;
#[allow(dead_code)]
pub const MESSAGE_NOT_INTERESTED: MessageId = 3;
pub const MESSAGE_HAVE: MessageId = 4;
pub const MESSAGE_BITFIELD: MessageId = 5;
pub const MESSAGE_REQUEST: MessageId = 6;
pub const MESSAGE_PIECE: MessageId = 7;
#[allow(dead_code)]
pub const MESSAGE_CANCEL: MessageId = 8;
pub const MESSAGE_KEEPALIVE: MessageId = 255; // Special value for keep-alive (length 0)

#[derive(Default, Debug)]
pub struct Message {
    /// Message type identifier
    pub id: MessageId,
    /// Message payload data
    pub payload: MessagePayload,
}

impl Message {
    /// Build a new message.
    ///
    /// # Arguments
    ///
    /// * `id` - The type of the message.
    ///
    pub fn new(id: MessageId) -> Self {
        Message {
            id,
            payload: vec![],
        }
    }

    /// Build a new message with a payload.
    ///
    /// # Arguments
    ///
    /// * `id` - The type of the message.
    /// * `payload` - The content of the message.
    ///
    pub fn new_with_payload(id: MessageId, payload: MessagePayload) -> Self {
        Message { id, payload }
    }

    /// Serialize message.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        // Get message length
        let message_len = 1 + self.payload.len();

        // Create a new buffer
        let mut serialized: Vec<u8> = vec![];

        // Add message length
        serialized.write_u32::<BigEndian>(message_len as u32)?;

        // Add message id
        serialized.push(self.id);

        // Add message payload
        let mut payload = self.payload.clone();
        serialized.append(&mut payload);

        Ok(serialized)
    }
}

/// Deserialize message.
///
/// # Arguments
///
/// * `message_buf` - The message to deserialize.
/// * `message_len` - The message length.
///
pub fn deserialize_message(message_buf: &[u8], message_len: usize) -> Result<Message> {
    // Get message id
    let id: MessageId = message_buf[0];
    // Get message payload
    let payload: MessagePayload = message_buf[1..message_len].to_vec();
    // Build message
    let message: Message = Message::new_with_payload(id, payload);

    Ok(message)
}
