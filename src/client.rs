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

use crate::handshake::*;
use crate::message::*;
use crate::peer::*;
use crate::piece::*;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, SocketAddr, Shutdown, TcpStream};
use std::time::Duration;

/// Client structure.
pub struct Client {
    // A peer
    peer: Peer,
    // Torrent peer id
    peer_id: Vec<u8>,
    // Torrent info hash
    info_hash: Vec<u8>,
    // Connection to peer
    conn: TcpStream,
    // Bitfield of pieces
    bitfield: Vec<u8>,
    // Peer has choked this client
    choked: bool,
}

impl Client {
    /// Build a new client.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - Urlencoded 20-byte string used as a unique ID for the client.
    /// * `info_hash` - 20-byte SHA-1 hash of the info key in the metainfo file.
    ///
    pub fn new(peer: Peer, peer_id: Vec<u8>, info_hash: Vec<u8>) -> Result<Client> {
        // Open connection with remote peer
        let peer_socket = SocketAddr::new(IpAddr::V4(peer.ip), peer.port);
        let conn = match TcpStream::connect_timeout(&peer_socket, Duration::from_secs(15)) {
            Ok(conn) => conn,
            Err(_) => return Err(anyhow!("could not connect to peer")),
        };

        info!("Connected to peer {:?}", peer.id);

        // Return new client
        let client = Client {
            peer,
            peer_id,
            info_hash,
            conn,
            bitfield: vec![],
            choked: true,
        };

        Ok(client)
    }

    // Return choked value.
    pub fn is_choked(&self) -> bool {
        self.choked
    }

    /// Check if peer has a piece.
    ///
    /// # Arguments
    ///
    /// * `index` - The piece index to check.
    ///
    pub fn has_piece(&self, index: u32) -> bool {
        let byte_index = index / 8;
        let offset = index % 8;

        // Prevent unbounded values
        if byte_index < self.bitfield.len() as u32 {
            // Check for piece index into bitfield
            return self.bitfield[byte_index as usize] >> (7 - offset) as u8 & 1 != 0;
        }
        false
    }

    /// Set a piece that peer has.
    ///
    /// # Arguments
    ///
    /// * `index` - The piece index to update into bitfield.
    ///
    pub fn set_piece(&mut self, index: u32) {
        let byte_index = index / 8;
        let offset = index % 8;

        // Create a new bitfield
        let mut bitfield: Vec<u8> = self.bitfield.to_vec();

        // Prevent unbounded values
        if byte_index < self.bitfield.len() as u32 {
            // Set piece index into bitfield
            bitfield[byte_index as usize] |= (1 << (7 - offset)) as u8;
            self.bitfield = bitfield;
        }
    }

    /// Set connection timeout.
    ///
    /// # Arguments
    ///
    /// * `secs` - The timeout in seconds.
    ///
    pub fn set_connection_timeout(&self, secs: u64) -> Result<()> {
        // Set write timeout
        if self
            .conn
            .set_write_timeout(Some(Duration::from_secs(secs)))
            .is_err()
        {
            return Err(anyhow!("could not set write timeout"));
        }

        // Set read timeout
        if self
            .conn
            .set_read_timeout(Some(Duration::from_secs(secs)))
            .is_err()
        {
            return Err(anyhow!("could not set read timeout"));
        }

        Ok(())
    }

    /// Handshake with remote peer.
    pub fn handshake_with_peer(&mut self) -> Result<()> {
        // Create handshake
        let peer_id = self.peer_id.clone();
        let info_hash = self.info_hash.clone();
        let handshake = Handshake::new(peer_id, info_hash);

        // Send handshake to remote peer
        let handshake_encoded: Vec<u8> = handshake.serialize()?;
        if self.conn.write(&handshake_encoded).is_err() {
            return Err(anyhow!("could not send handshake to peer"));
        }

        // Read handshake received from remote peer
        let handshake_len: usize = self.read_handshake_len()?;
        let mut handshake_buf: Vec<u8> = vec![0; 48 + handshake_len];
        if self.conn.read_exact(&mut handshake_buf).is_err() {
            return Err(anyhow!("could not read handshake received from peer"));
        }

        // Check info hash received from remote peer
        let handshake_decoded: Handshake = deserialize_handshake(&handshake_buf, handshake_len)?;
        if handshake_decoded.info_hash != self.info_hash {
            return Err(anyhow!("invalid handshake received from peer"));
        }

        Ok(())
    }

    /// Read handshake length.
    fn read_handshake_len(&mut self) -> Result<usize> {
        // Read 1 byte into buffer
        let mut buf = [0; 1];
        if self.conn.read_exact(&mut buf).is_err() {
            return Err(anyhow!(
                "could not read handshake length received from peer"
            ));
        }

        // Get handshake length
        let handshake_len = buf[0];
        if handshake_len == 0 {
            return Err(anyhow!("invalid handshake length received from peer"));
        }

        Ok(handshake_len as usize)
    }

    /// Read message from remote peer.
    pub fn read_message(&mut self) -> Result<Message> {
        let message_len: usize = self.read_message_len()?;

        // If message length is 0, it's a keep-alive
        if message_len == 0 {
            info!("Receive KEEP_ALIVE from peer {:?}", self.peer.id);
            return Err(anyhow!("keep-alive"));
        }

        // Read message
        let mut message_buf: Vec<u8> = vec![0; message_len];
        if self.conn.read_exact(&mut message_buf).is_err() {
            return Err(anyhow!("could not read message received from peer"));
        }

        // Deserialize message
        let message: Message = deserialize_message(&message_buf, message_len)?;

        Ok(message)
    }

    /// Read message length.
    fn read_message_len(&mut self) -> Result<usize> {
        // Read bytes into buffer
        let mut buf = vec![0; 4];
        if self.conn.read_exact(&mut buf).is_err() {
            return Err(anyhow!("could not read message length received from peer"));
        }

        // Get message length
        let mut cursor = Cursor::new(buf);
        let message_len = cursor.read_u32::<BigEndian>()?;

        Ok(message_len as usize)
    }

    /// Read CHOKE message from remote peer.
    pub fn read_choke(&mut self) {
        info!("Receive MESSAGE_CHOKE from peer {:?}", self.peer.id);
        self.choked = true
    }

    /// Send UNCHOKE message to remote peer.
    pub fn send_unchoke(&mut self) -> Result<()> {
        let message: Message = Message::new(MESSAGE_UNCHOKE);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_UNCHOKE to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_UNCHOKE to peer"));
        }

        Ok(())
    }

    /// Read UNCHOKE message from remote peer.
    pub fn read_unchoke(&mut self) {
        info!("Receive MESSAGE_UNCHOKE from peer {:?}", self.peer.id);
        self.choked = false
    }

    /// Send INTERESTED message to remote peer.
    pub fn send_interested(&mut self) -> Result<()> {
        let message: Message = Message::new(MESSAGE_INTERESTED);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_INTERESTED to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_INTERESTED to peer"));
        }

        Ok(())
    }

    /// Send HAVE message to remote peer.
    ///
    /// # Arguments
    ///
    /// * `index` - The index of a piece that has just been successfully downloaded and verified.
    ///
    pub fn send_have(&mut self, index: u32) -> Result<()> {
        let mut payload: Vec<u8> = vec![];
        payload.write_u32::<BigEndian>(index)?;

        let message: Message = Message::new_with_payload(MESSAGE_HAVE, payload);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_HAVE to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_HAVE to peer"));
        }

        Ok(())
    }

    /// Read HAVE message from remote peer.
    ///
    /// The message payload is the zero-based index of a piece that has just been successfully downloaded and verified via the hash.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to parse.
    ///
    pub fn read_have(&mut self, message: Message) -> Result<()> {
        info!("Receive MESSAGE_HAVE from peer {:?}", self.peer.id);

        // Check if message id and payload are valid
        if message.id != MESSAGE_HAVE || message.payload.to_vec().len() != 4 {
            return Err(anyhow!("received invalid MESSAGE_HAVE from peer"));
        }

        // Get piece index
        let mut payload_cursor = Cursor::new(message.payload.to_vec());
        let index = payload_cursor.read_u32::<BigEndian>()?;

        // Update bitfield
        self.set_piece(index);

        Ok(())
    }

    /// Read BITFIELD message from remote peer.
    ///
    /// The message payload is a bitfield representing the pieces that have been successfully downloaded.
    /// The high bit in the first byte corresponds to piece index 0.
    /// Bits that are cleared indicated a missing piece, and set bits indicate a valid and available piece.
    /// Spare bits at the end are set to zero.
    ///
    pub fn read_bitfield(&mut self) -> Result<()> {
        info!("Receive MESSAGE_BITFIELD from peer {:?}", self.peer.id);

        let message: Message = self.read_message()?;
        if message.id != MESSAGE_BITFIELD {
            return Err(anyhow!("received invalid MESSAGE_BITFIELD from peer"));
        }

        // Update bitfield
        self.bitfield = message.payload.to_vec();

        Ok(())
    }

    /// Send REQUEST message to remote peer.
    ///
    /// The request message is fixed length, and is used to request a block.
    ///
    /// # Arguments
    ///
    /// * `index` - The zero-based piece index.
    /// * `begin` - The zero-based byte offset within the piece.
    /// * `length` - The requested length.
    ///
    pub fn send_request(&mut self, index: u32, begin: u32, length: u32) -> Result<()> {
        let mut payload: Vec<u8> = vec![];
        payload.write_u32::<BigEndian>(index)?;
        payload.write_u32::<BigEndian>(begin)?;
        payload.write_u32::<BigEndian>(length)?;

        let message: Message = Message::new_with_payload(MESSAGE_REQUEST, payload);
        let message_encoded = message.serialize()?;

        info!(
            "Send MESSAGE_REQUEST for piece {:?} [{:?}:{:?}] to peer {:?}",
            index,
            begin,
            begin + length,
            self.peer.id
        );

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_REQUEST to peer"));
        }

        Ok(())
    }

    /// Read PIECE message from remote peer.
    ///
    /// The message payload contains the following information:
    /// - index: integer specifying the zero-based piece index
    /// - begin: integer specifying the zero-based byte offset within the piece
    /// - block: block of data, which is a subset of the piece specified by index.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to parse.
    /// * `piece_work` - A work piece.
    ///
    pub fn read_piece(&mut self, message: Message, piece_work: &mut PieceWork) -> Result<()> {
        info!("Receive MESSAGE_PIECE from peer {:?}", self.peer.id);

        // Check if message id and payload are valid
        if message.id != MESSAGE_PIECE || message.payload.to_vec().len() < 8 {
            return Err(anyhow!("received invalid MESSAGE_HAVE from peer"));
        }

        // Get message payload
        let payload: Vec<u8> = message.payload.to_vec();

        // Get piece index
        let mut payload_cursor = Cursor::new(&payload[0..4]);
        let index = payload_cursor.read_u32::<BigEndian>()?;

        // Check if piece index is valid
        if index != piece_work.index {
            return Err(anyhow!("received invalid piece from peer"));
        }

        // Get byte offset within piece
        let mut payload_cursor = Cursor::new(&payload[4..8]);
        let begin: u32 = payload_cursor.read_u32::<BigEndian>()?;

        // Get piece block
        let block: Vec<u8> = payload[8..].to_vec();
        let block_len: u32 = block.len() as u32;

        // Check if byte offset is valid
        if begin + block_len > piece_work.length {
            return Err(anyhow!(
                "received invalid byte offset within piece from peer"
            ));
        }

        info!(
            "Download piece {:?} [{:?}:{:?}] from peer {:?}",
            index,
            begin,
            begin + block_len,
            self.peer.id
        );

        // Add block to piece data
        for i in 0..block_len {
            piece_work.data[begin as usize + i as usize] = block[i as usize];
        }

        // Update downloaded data counter
        piece_work.downloaded += block_len;

        // Update requests counter
        piece_work.requests -= 1;

        Ok(())
    }

    /// Reconnect to the peer.
    pub fn reconnect(&mut self) -> Result<()> {
        info!("Attempting to reconnect to peer {:?}", self.peer.id);

        // Close existing connection if any
        if let Err(e) = self.conn.shutdown(Shutdown::Both) {
            warn!("Error shutting down existing connection: {}", e);
        }

        // Create new connection
        let peer_socket = SocketAddr::new(IpAddr::V4(self.peer.ip), self.peer.port);
        let new_conn = match TcpStream::connect(peer_socket) {
            Ok(conn) => conn,
            Err(_) => return Err(anyhow!("could not reconnect to peer")),
        };

        // Set connection timeout
        if new_conn.set_read_timeout(Some(Duration::from_secs(30))).is_err() {
            return Err(anyhow!("could not set read timeout on new connection"));
        }

        if new_conn.set_write_timeout(Some(Duration::from_secs(30))).is_err() {
            return Err(anyhow!("could not set write timeout on new connection"));
        }

        // Replace old connection
        self.conn = new_conn;
        self.choked = true; // Reset choke state

        info!("Successfully reconnected to peer {:?}", self.peer.id);

        Ok(())
    }
}
