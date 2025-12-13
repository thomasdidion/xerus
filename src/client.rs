//! # BitTorrent Peer Client
//!
//! This module implements the client-side peer wire protocol for BitTorrent,
//! handling TCP connections, message exchange, and piece downloading from remote peers.
//!
//! ## Protocol Overview
//!
//! The peer wire protocol operates over TCP and consists of:
//!
//! 1. **Handshake**: Initial protocol negotiation with peer identification
//! 2. **Bitfield Exchange**: Sharing which pieces each peer has
//! 3. **Choke/Unchoke Management**: Flow control for download rates
//! 4. **Piece Requests**: Requesting specific blocks of data
//! 5. **Piece Transfer**: Receiving and validating downloaded pieces
//!
//! ## Message Format
//!
//! All messages follow a common format:
//!
//! ```text
//! <length prefix><message ID><payload>
//! ```
//!
//! - Length prefix: 4 bytes (big-endian u32)
//! - Message ID: 1 byte
//! - Payload: Variable length (length - 1 bytes)
//!
//! ## Bitfield Encoding
//!
//! The bitfield is a compact representation of piece availability:
//!
//! - Each byte represents 8 pieces
//! - Bit 7 (MSB) = piece index 0, bit 0 (LSB) = piece index 7
//! - Set bits indicate available pieces, clear bits indicate missing pieces
//! - Trailing bits in the last byte are set to 0
//!
//! ## Connection States
//!
//! Peers can be in various states affecting download capability:
//!
//! - **Choked**: Peer won't send requested pieces
//! - **Interested**: Client wants to download from this peer
//! - **Unchoked**: Peer will respond to piece requests
//!
//! ## Error Handling
//!
//! The client implements robust error handling with:
//!
//! - Connection timeouts and reconnection logic
//! - Message validation and parsing error detection
//! - Piece integrity verification via SHA-1 hashing
//! - Graceful degradation when peers become unavailable

use crate::handshake::*;
use crate::message::*;
use crate::peer::*;
use crate::piece::*;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

/// Represents a connection to a remote BitTorrent peer.
///
/// Manages the TCP connection, protocol state, and piece downloading for a single peer.
/// Each client instance corresponds to one peer in the swarm and handles all communication
/// with that peer according to the BitTorrent peer wire protocol.
pub struct Client {
    /// Information about the remote peer (IP, port, ID)
    peer: Peer,
    /// 20-byte unique identifier for this client instance
    peer_id: Vec<u8>,
    /// 20-byte SHA-1 hash of the torrent's info dictionary
    info_hash: Vec<u8>,
    /// TCP stream connection to the peer
    conn: TcpStream,
    /// Bitfield indicating which pieces the peer has (compact boolean array)
    bitfield: Vec<u8>,
    /// Whether the peer has choked this client (preventing downloads)
    choked: bool,
}

impl Client {
    /// Creates a new client instance and establishes TCP connection to a peer.
    ///
    /// This involves:
    /// 1. Creating a socket address from peer IP and port
    /// 2. Establishing TCP connection with 15-second timeout
    /// 3. Initializing client state (choked, empty bitfield)
    ///
    /// # Arguments
    ///
    /// * `peer` - Peer information including IP address and port
    /// * `peer_id` - 20-byte unique identifier for this client (randomly generated)
    /// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
    ///
    /// # Returns
    ///
    /// Returns a `Result<Client>` with the connected client or an error if connection fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - TCP connection cannot be established within timeout
    /// - Socket address creation fails
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

    /// Returns whether this client is choked by the peer.
    ///
    /// A choked client cannot request pieces from the peer until unchoked.
    /// This is part of BitTorrent's flow control mechanism.
    pub fn is_choked(&self) -> bool {
        self.choked
    }

    /// Checks if the peer has a specific piece available for download.
    ///
    /// Performs bitfield lookup using compact bit array representation.
    /// Each byte in the bitfield represents 8 pieces, with bits ordered from MSB to LSB.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index to check
    ///
    /// # Returns
    ///
    /// `true` if the peer has the piece, `false` otherwise or if index is out of bounds.
    ///
    /// # Bitfield Format
    ///
    /// ```text
    /// Byte 0: [piece 7, 6, 5, 4, 3, 2, 1, 0]
    /// Byte 1: [piece 15, 14, 13, 12, 11, 10, 9, 8]
    /// ...
    /// ```
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

    /// Marks a piece as available in the peer's bitfield.
    ///
    /// Updates the compact bit array representation. Automatically resizes the bitfield
    /// if the piece index exceeds current capacity.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index to mark as available
    ///
    /// # Bitfield Growth
    ///
    /// The bitfield grows dynamically: if piece index 100 is set but bitfield only
    /// has space for 64 pieces (8 bytes), it will be extended to accommodate.
    pub fn set_piece(&mut self, index: u32) {
        let byte_index = index / 8;
        let offset = index % 8;

        // Create a new bitfield
        let mut bitfield: Vec<u8> = self.bitfield.to_vec();

        // Resize bitfield if needed to accommodate the piece index
        if byte_index >= bitfield.len() as u32 {
            let additional_bytes = (byte_index as usize) - bitfield.len() + 1;
            bitfield.extend(vec![0; additional_bytes]);
        }

        // Set piece index into bitfield
        bitfield[byte_index as usize] |= (1 << (7 - offset)) as u8;
        self.bitfield = bitfield;
    }

    /// Sets read and write timeouts on the TCP connection.
    ///
    /// Prevents the client from hanging indefinitely on slow or unresponsive peers.
    /// Both read and write timeouts are set to the same value.
    ///
    /// # Arguments
    ///
    /// * `secs` - Timeout duration in seconds
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if timeouts are set successfully, or an error if the operation fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying TCP stream doesn't support timeout configuration.
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

    /// Performs the BitTorrent handshake protocol with the remote peer.
    ///
    /// The handshake consists of:
    /// 1. Sending our handshake message (protocol, reserved bytes, info_hash, peer_id)
    /// 2. Receiving peer's handshake response
    /// 3. Validating the peer's info_hash matches ours
    ///
    /// # Handshake Message Format
    ///
    /// ```text
    /// <pstrlen><pstr><reserved><info_hash><peer_id>
    /// ```
    ///
    /// - pstrlen: 1 byte (length of pstr, usually 19)
    /// - pstr: variable length protocol string ("BitTorrent protocol")
    /// - reserved: 8 bytes (all zeros)
    /// - info_hash: 20 bytes
    /// - peer_id: 20 bytes
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if handshake succeeds, or an error if protocol negotiation fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Handshake serialization fails
    /// - Network send/receive fails
    /// - Peer sends invalid handshake (wrong info_hash)
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

    /// Reads the first byte of the peer's handshake to determine protocol string length.
    ///
    /// The first byte indicates how many bytes follow for the protocol identifier.
    /// For standard BitTorrent, this should be 19 (length of "BitTorrent protocol").
    ///
    /// # Returns
    ///
    /// Returns the protocol string length as `usize`, or an error if reading fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Network read fails
    /// - Received length is 0 (invalid)
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

    /// Reads and parses a message from the peer according to the peer wire protocol.
    ///
    /// Messages have a 4-byte big-endian length prefix, followed by the message ID and payload.
    /// Length 0 indicates a keep-alive message (no ID or payload).
    ///
    /// # Message Format
    ///
    /// ```text
    /// <length: u32><id: u8><payload: [u8]>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns a parsed `Message` struct, or an error if reading/parsing fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Network read fails
    /// - Message deserialization fails
    /// - Invalid message format received
    pub fn read_message(&mut self) -> Result<Message> {
        let message_len: usize = self.read_message_len()?;

        // If message length is 0, it's a keep-alive
        if message_len == 0 {
            info!("Receive KEEP_ALIVE from peer {:?}", self.peer.id);
            return Ok(Message::new(MESSAGE_KEEPALIVE));
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

    /// Reads the 4-byte length prefix of an incoming message.
    ///
    /// The length prefix indicates the total bytes following (message ID + payload).
    /// Uses big-endian byte order as per BitTorrent specification.
    ///
    /// # Returns
    ///
    /// Returns the message length in bytes, or an error if reading fails.
    ///
    /// # Errors
    ///
    /// Returns an error if network read fails or data cannot be parsed as u32.
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

    /// Processes a CHOKE message from the peer.
    ///
    /// When choked, the peer will not respond to piece requests from this client.
    /// This is part of BitTorrent's flow control mechanism.
    pub fn read_choke(&mut self) {
        info!("Receive MESSAGE_CHOKE from peer {:?}", self.peer.id);
        self.choked = true
    }

    /// Sends an UNCHOKE message to the peer.
    ///
    /// Signals that we are willing to accept piece requests from this peer.
    /// In practice, most clients unchoke all peers immediately after handshake.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if message is sent successfully, or an error if network send fails.
    pub fn send_unchoke(&mut self) -> Result<()> {
        let message: Message = Message::new(MESSAGE_UNCHOKE);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_UNCHOKE to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_UNCHOKE to peer"));
        }

        Ok(())
    }

    /// Processes an UNCHOKE message from the peer.
    ///
    /// When unchoked, the peer will respond to our piece requests.
    /// This allows the download process to begin or resume.
    pub fn read_unchoke(&mut self) {
        info!("Receive MESSAGE_UNCHOKE from peer {:?}", self.peer.id);
        self.choked = false
    }

    /// Sends an INTERESTED message to the peer.
    ///
    /// Signals that we are interested in downloading pieces from this peer.
    /// Required before sending REQUEST messages.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if message is sent successfully, or an error if network send fails.
    pub fn send_interested(&mut self) -> Result<()> {
        let message: Message = Message::new(MESSAGE_INTERESTED);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_INTERESTED to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_INTERESTED to peer"));
        }

        Ok(())
    }

    /// Sends a NOT INTERESTED message to indicate we don't want to download from this peer.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if message is sent successfully, or an error if network send fails.
    #[allow(dead_code)]
    pub fn send_not_interested(&mut self) -> Result<()> {
        let message: Message = Message::new(MESSAGE_NOT_INTERESTED);
        let message_encoded = message.serialize()?;

        info!("Send MESSAGE_NOT_INTERESTED to peer {:?}", self.peer.id);

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_NOT_INTERESTED to peer"));
        }

        Ok(())
    }

    /// Sends a HAVE message to notify the peer that we now have a piece.
    ///
    /// This informs other peers in the swarm about our piece availability,
    /// helping with rarest-first piece selection algorithms.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based index of the piece we successfully downloaded and verified
    ///
    /// # Message Format
    ///
    /// ```text
    /// <len=0005><id=4><piece index: u32>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if message is sent successfully, or an error if network send fails.
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

    /// Processes a HAVE message from the peer and updates their bitfield.
    ///
    /// The peer is notifying us that they now have a specific piece available.
    /// We update our record of their piece availability for future download decisions.
    ///
    /// # Arguments
    ///
    /// * `message` - HAVE message containing the piece index in payload
    ///
    /// # Message Format
    ///
    /// ```text
    /// <len=0005><id=4><piece index: u32>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the message is processed successfully, or an error if the message format is invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if the message payload doesn't contain exactly 4 bytes for the piece index.
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

    /// Reads and processes the peer's BITFIELD message containing their piece availability.
    ///
    /// The bitfield is a compact representation showing which pieces the peer has.
    /// This is typically sent immediately after handshake and before other messages.
    ///
    /// # Bitfield Format
    ///
    /// ```text
    /// <len><id=5><bitfield bytes>
    /// ```
    ///
    /// Each byte represents 8 pieces:
    /// - Bit 7 (MSB) = piece index 0
    /// - Bit 6 = piece index 1
    /// - ...
    /// - Bit 0 (LSB) = piece index 7
    ///
    /// Set bits indicate available pieces, clear bits indicate missing pieces.
    /// Unused bits in the final byte are set to zero.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if bitfield is read and stored successfully, or an error if reading fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the message ID is not BITFIELD or network read fails.
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

    /// Sends a REQUEST message to ask the peer for a specific block of data.
    ///
    /// Pieces are downloaded in smaller blocks (typically 16KB) to allow parallel downloading
    /// and to handle network interruptions gracefully. Multiple REQUESTs can be pipelined.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index
    /// * `begin` - Zero-based byte offset within the piece
    /// * `length` - Number of bytes to request (usually 2^14 = 16384)
    ///
    /// # Message Format
    ///
    /// ```text
    /// <len=0013><id=6><index: u32><begin: u32><length: u32>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if request is sent successfully, or an error if network send fails.
    ///
    /// # Protocol Notes
    ///
    /// - Client must be unchoked and interested to send requests
    /// - Peer must have the requested piece
    /// - Request size should not exceed 2^14 bytes
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

    /// Sends a CANCEL message to cancel a pending request.
    ///
    /// This is used during endgame mode to prevent duplicate downloads of the same block.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index
    /// * `begin` - Zero-based byte offset within the piece
    /// * `length` - Number of bytes to cancel (usually 2^14 = 16384)
    ///
    /// # Message Format
    ///
    /// ```text
    /// <len=0013><id=8><index: u32><begin: u32><length: u32>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if cancel is sent successfully, or an error if network send fails.
    #[allow(dead_code)]
    pub fn send_cancel(&mut self, index: u32, begin: u32, length: u32) -> Result<()> {
        let mut payload: Vec<u8> = vec![];
        payload.write_u32::<BigEndian>(index)?;
        payload.write_u32::<BigEndian>(begin)?;
        payload.write_u32::<BigEndian>(length)?;

        let message: Message = Message::new_with_payload(MESSAGE_CANCEL, payload);
        let message_encoded = message.serialize()?;

        info!(
            "Send MESSAGE_CANCEL for piece {:?} [{:?}:{:?}] to peer {:?}",
            index,
            begin,
            begin + length,
            self.peer.id
        );

        if self.conn.write(&message_encoded).is_err() {
            return Err(anyhow!("could not send MESSAGE_CANCEL to peer"));
        }

        Ok(())
    }

    /// Processes a PIECE message containing requested block data from the peer.
    ///
    /// This is the response to a REQUEST message, containing the actual file data.
    /// The block is copied into the appropriate position in the piece buffer.
    ///
    /// # Arguments
    ///
    /// * `message` - PIECE message with payload containing index, begin, and block data
    /// * `piece_work` - Piece work structure to update with received data
    ///
    /// # Message Format
    ///
    /// ```text
    /// <len><id=7><index: u32><begin: u32><block: [u8]>
    /// ```
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the piece block is processed successfully, or an error if validation fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Message payload is too short (< 8 bytes)
    /// - Piece index doesn't match expected piece
    /// - Block offset exceeds piece boundaries
    /// - Block size is invalid
    ///
    /// # Data Integrity
    ///
    /// The received block is copied directly into the piece buffer at the specified offset.
    /// Integrity verification happens later via SHA-1 hash of the complete piece.
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

    /// Attempts to reconnect to the peer after a connection failure.
    ///
    /// Closes the existing connection gracefully, establishes a new TCP connection,
    /// and sets up timeouts. The choke state is reset since it's connection-specific.
    ///
    /// # Reconnection Process
    ///
    /// 1. Shutdown existing connection (both read/write)
    /// 2. Create new TCP connection to peer
    /// 3. Set read/write timeouts (30 seconds)
    /// 4. Reset choke state to true
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if reconnection succeeds, or an error if connection fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - TCP connection cannot be established
    /// - Timeout configuration fails
    /// - Peer is no longer reachable
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
        if new_conn
            .set_read_timeout(Some(Duration::from_secs(30)))
            .is_err()
        {
            return Err(anyhow!("could not set read timeout on new connection"));
        }

        if new_conn
            .set_write_timeout(Some(Duration::from_secs(30)))
            .is_err()
        {
            return Err(anyhow!("could not set write timeout on new connection"));
        }

        // Replace old connection
        self.conn = new_conn;
        self.choked = true; // Reset choke state

        info!("Successfully reconnected to peer {:?}", self.peer.id);

        Ok(())
    }
}
