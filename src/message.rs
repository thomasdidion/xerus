// Copyright (c) 2020 zenoxygen
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use anyhow::Result;
use byteorder::{BigEndian, WriteBytesExt};

type MessageId = u8;
type MessagePayload = Vec<u8>;

pub const MESSAGE_CHOKE: MessageId = 0;
pub const MESSAGE_UNCHOKE: MessageId = 1;
pub const MESSAGE_INTERESTED: MessageId = 2;
pub const MESSAGE_HAVE: MessageId = 4;
pub const MESSAGE_BITFIELD: MessageId = 5;
pub const MESSAGE_REQUEST: MessageId = 6;
pub const MESSAGE_PIECE: MessageId = 7;
pub const MESSAGE_KEEPALIVE: MessageId = 255; // Special value for keep-alive (length 0)

#[derive(Default, Debug)]
pub struct Message {
    // Message type
    pub id: MessageId,
    // Message payload
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
