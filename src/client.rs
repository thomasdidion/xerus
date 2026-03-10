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
use serde::{Deserialize, Serialize};
use serde_bencode::{de, ser};
use std::collections::HashMap;

use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

// TCP connection timeout in seconds
const CONNECT_TIMEOUT_SECS: u64 = 5;
// Default read/write timeout in seconds
const IO_TIMEOUT_SECS: u64 = 10;
// Metadata piece size (16KB, same as block size)
const METADATA_PIECE_SIZE: u32 = 16384;
// Maximum metadata size to accept from peers (16 MiB)
const MAX_METADATA_SIZE: u32 = 16 * 1024 * 1024;

// BEP 9 metadata message types
const METADATA_MSG_REQUEST: u8 = 0;
const METADATA_MSG_DATA: u8 = 1;
const METADATA_MSG_REJECT: u8 = 2;

/// Extension handshake message structure (BEP 10).
#[derive(Debug, Serialize, Deserialize, Default)]
struct ExtensionHandshake {
    /// Extension message ID mappings (e.g. {"ut_metadata": 1})
    #[serde(rename = "m", default)]
    extensions: HashMap<String, u8>,
    /// Size of torrent metadata (only present if peer has it)
    #[serde(default)]
    metadata_size: Option<u32>,
}

/// Metadata message structure (BEP 9).
///
/// Used for requesting and receiving torrent metadata pieces from peers.
/// Message types: 0 = request, 1 = data, 2 = reject.
#[derive(Debug, Serialize, Deserialize)]
struct MetadataMessage {
    /// Message type
    msg_type: u8,
    /// Metadata piece index
    piece: u32,
    /// Total metadata size in bytes (present in data responses)
    #[serde(default)]
    total_size: Option<u32>,
}

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
    /// Whether the peer supports extension protocol (BEP 10)
    supports_extensions: bool,
    /// Peer's ut_metadata extension ID (0 if not supported)
    peer_ut_metadata_id: u8,
    /// Size of torrent metadata in bytes (from peer's extension handshake)
    peer_metadata_size: u32,
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
        let conn = match TcpStream::connect_timeout(
            &peer_socket,
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
        ) {
            Ok(conn) => conn,
            Err(_) => return Err(anyhow!("could not connect to peer")),
        };

        // Set timeouts immediately after connection
        if conn
            .set_read_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))
            .is_err()
        {
            return Err(anyhow!("could not set read timeout"));
        }
        if conn
            .set_write_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))
            .is_err()
        {
            return Err(anyhow!("could not set write timeout"));
        }

        info!("Connected to peer {:?}", peer.id);

        // Return new client
        let client = Client {
            peer,
            peer_id,
            info_hash,
            conn,
            bitfield: vec![],
            choked: true,
            supports_extensions: false,
            peer_ut_metadata_id: 0,
            peer_metadata_size: 0,
        };

        Ok(client)
    }

    /// Returns whether the peer supports extension protocol (BEP 10).
    pub fn supports_extensions(&self) -> bool {
        self.supports_extensions
    }

    /// Returns true if peer supports ut_metadata extension (BEP 9).
    pub fn supports_ut_metadata(&self) -> bool {
        self.peer_ut_metadata_id != 0
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
        self.conn
            .write_all(&handshake_encoded)
            .map_err(|e| anyhow!("could not send handshake to peer: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| anyhow!("could not flush handshake to peer: {}", e))?;

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

        // Check if peer supports extension protocol (BEP 10)
        self.supports_extensions = handshake_decoded.supports_extensions();

        Ok(())
    }

    /// Sends extension handshake to negotiate ut_metadata support (BEP 10).
    ///
    /// The extension handshake is sent as an extended message (ID 20) with
    /// extension message ID 0, containing a bencoded dictionary of supported extensions.
    pub fn send_extension_handshake(&mut self) -> Result<()> {
        if !self.supports_extensions {
            return Ok(());
        }

        // Build extension handshake: {m: {ut_metadata: 1}}
        let mut extensions = HashMap::new();
        extensions.insert("ut_metadata".to_string(), 1u8);
        let handshake = ExtensionHandshake {
            extensions,
            metadata_size: None,
        };

        let payload_dict = ser::to_bytes(&handshake)?;

        // Extended message format: <msg_id=20><ext_msg_id=0><payload>
        let mut payload = vec![0u8]; // Extension message ID 0 = handshake
        payload.extend(payload_dict);

        let message = Message::new_with_payload(MESSAGE_EXTENDED, payload);
        let encoded = message.serialize()?;

        self.conn
            .write_all(&encoded)
            .map_err(|e| anyhow!("could not send extension handshake: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| anyhow!("could not flush extension handshake: {}", e))?;

        info!("Sent extension handshake to peer {:?}", self.peer.id);
        Ok(())
    }

    /// Reads and parses extension handshake from peer (BEP 10).
    ///
    /// Extracts the peer's ut_metadata extension ID and metadata_size.
    /// Handles other messages (bitfield, have, etc.) that may arrive first.
    pub fn read_extension_handshake(&mut self) -> Result<()> {
        // Limit messages to read to prevent infinite loops
        const MAX_MESSAGES: u32 = 100;

        for _ in 0..MAX_MESSAGES {
            let message = self.read_message()?;

            match message.id {
                MESSAGE_BITFIELD => {
                    self.bitfield = message.payload.to_vec();
                    continue;
                }
                MESSAGE_HAVE => {
                    let _ = self.read_have(message);
                    continue;
                }
                MESSAGE_CHOKE => {
                    self.read_choke();
                    continue;
                }
                MESSAGE_UNCHOKE => {
                    self.read_unchoke();
                    continue;
                }
                MESSAGE_EXTENDED if !message.payload.is_empty() && message.payload[0] == 0 => {
                    // This is the extension handshake we're looking for
                    return self.parse_extension_handshake(&message.payload);
                }
                MESSAGE_KEEPALIVE => continue,
                _ => {
                    // Ignore other messages and keep waiting for extension handshake
                    // Peers may send various messages before the extension handshake
                    continue;
                }
            }
        }

        Err(anyhow!(
            "no extension handshake received after {} messages",
            MAX_MESSAGES
        ))
    }

    /// Parse the extension handshake payload (BEP 10).
    ///
    /// Extracts the peer's ut_metadata extension ID and metadata_size
    /// from the bencoded dictionary.
    ///
    /// # Arguments
    ///
    /// * `payload` - The extension message payload (first byte is the extension message ID).
    ///
    fn parse_extension_handshake(&mut self, payload: &[u8]) -> Result<()> {
        // Parse bencoded dictionary (skip first byte which is the extension message ID)
        let handshake: ExtensionHandshake = de::from_bytes(&payload[1..])?;

        // Extract ut_metadata extension ID
        if let Some(&id) = handshake.extensions.get("ut_metadata") {
            self.peer_ut_metadata_id = id;
        }

        // Extract metadata size
        if let Some(size) = handshake.metadata_size {
            self.peer_metadata_size = size;
        }

        info!(
            "Peer {:?} ut_metadata={}, metadata_size={}",
            self.peer.id, self.peer_ut_metadata_id, self.peer_metadata_size
        );

        Ok(())
    }

    /// Request a metadata piece from the peer (BEP 9).
    ///
    /// Sends a ut_metadata request message for the specified piece index.
    ///
    /// # Arguments
    ///
    /// * `piece` - The metadata piece index to request.
    ///
    pub fn request_metadata_piece(&mut self, piece: u32) -> Result<()> {
        if self.peer_ut_metadata_id == 0 {
            return Err(anyhow!("peer does not support ut_metadata"));
        }

        let request = MetadataMessage {
            msg_type: METADATA_MSG_REQUEST,
            piece,
            total_size: None,
        };

        let request_bytes = ser::to_bytes(&request)?;

        // Extended message: <msg_id=20><peer's ut_metadata id><bencoded request>
        let mut payload = vec![self.peer_ut_metadata_id];
        payload.extend(request_bytes);

        let message = Message::new_with_payload(MESSAGE_EXTENDED, payload);
        let encoded = message.serialize()?;

        self.conn
            .write_all(&encoded)
            .map_err(|e| anyhow!("could not send metadata request: {}", e))?;
        self.conn
            .flush()
            .map_err(|e| anyhow!("could not flush metadata request: {}", e))?;

        info!(
            "Requested metadata piece {} from peer {:?}",
            piece, self.peer.id
        );
        Ok(())
    }

    /// Reads a metadata piece response from the peer (BEP 9).
    ///
    /// Returns the piece index and data, or an error if rejected.
    /// Handles other messages (HAVE, CHOKE, etc.) that may arrive before the response.
    pub fn read_metadata_piece(&mut self) -> Result<(u32, Vec<u8>)> {
        const MAX_MESSAGES: u32 = 100;

        for _ in 0..MAX_MESSAGES {
            let message = self.read_message()?;

            match message.id {
                MESSAGE_BITFIELD => {
                    self.bitfield = message.payload.to_vec();
                    continue;
                }
                MESSAGE_HAVE => {
                    let _ = self.read_have(message);
                    continue;
                }
                MESSAGE_CHOKE => {
                    self.read_choke();
                    continue;
                }
                MESSAGE_UNCHOKE => {
                    self.read_unchoke();
                    continue;
                }
                MESSAGE_KEEPALIVE => continue,
                MESSAGE_EXTENDED if !message.payload.is_empty() => {
                    // Check if this is a ut_metadata response (peer uses OUR advertised ID, which is 1)
                    if message.payload[0] != 1 {
                        // Extension handshake or other extension message, skip
                        continue;
                    }

                    // Find the end of bencoded dict
                    let dict_end =
                        Self::bencode_value_len(&message.payload[1..]).ok_or_else(|| {
                            anyhow!("invalid metadata response: could not find dict end")
                        })? + 1; // +1 for the offset from payload[1..]

                    // Parse the bencoded header
                    let meta: MetadataMessage = de::from_bytes(&message.payload[1..dict_end])?;

                    if meta.msg_type == METADATA_MSG_REJECT {
                        return Err(anyhow!("metadata request rejected by peer"));
                    }

                    if meta.msg_type != METADATA_MSG_DATA {
                        // Not a data response, continue waiting
                        continue;
                    }

                    // Data follows the bencoded dict
                    let data = message.payload[dict_end..].to_vec();

                    info!(
                        "Received metadata piece {} ({} bytes)",
                        meta.piece,
                        data.len()
                    );
                    return Ok((meta.piece, data));
                }
                _ => {
                    // Ignore unexpected messages and keep waiting
                    continue;
                }
            }
        }

        Err(anyhow!(
            "no metadata response after {} messages",
            MAX_MESSAGES
        ))
    }

    /// Find the byte length of a bencoded value.
    ///
    /// Walks through the bencoded data iteratively using a depth counter
    /// to track nested containers (dicts/lists). Returns the position
    /// just past the end of the outermost value.
    ///
    /// # Arguments
    ///
    /// * `data` - The bencoded data starting at a value boundary.
    ///
    fn bencode_value_len(data: &[u8]) -> Option<usize> {
        let mut pos = 0;
        let mut depth = 0;

        loop {
            match *data.get(pos)? {
                // Dict or list opener: increase nesting depth
                b'd' | b'l' => {
                    depth += 1;
                    pos += 1;
                }
                // Container closer: decrease depth, done if back to zero
                b'e' => {
                    depth -= 1;
                    pos += 1;
                    if depth == 0 {
                        return Some(pos);
                    }
                }
                // Integer: skip from 'i' to closing 'e'
                b'i' => {
                    pos += 2 + data[pos + 1..].iter().position(|&b| b == b'e')?;
                }
                // Byte string: parse length prefix, skip ':' + content
                b'0'..=b'9' => {
                    let colon = data[pos..].iter().position(|&b| b == b':')?;
                    let len: usize = std::str::from_utf8(&data[pos..pos + colon])
                        .ok()?
                        .parse()
                        .ok()?;
                    pos += colon + 1 + len;
                }
                _ => return None,
            }
        }
    }

    /// Downloads complete metadata from peer (BEP 9).
    ///
    /// Requests all metadata pieces and assembles them into the complete info dictionary.
    pub fn download_metadata(&mut self) -> Result<Vec<u8>> {
        if self.peer_metadata_size == 0 {
            return Err(anyhow!("peer has no metadata"));
        }
        if self.peer_metadata_size > MAX_METADATA_SIZE {
            return Err(anyhow!(
                "metadata too large ({} bytes)",
                self.peer_metadata_size
            ));
        }

        let num_pieces = self.peer_metadata_size.div_ceil(METADATA_PIECE_SIZE);
        let mut metadata = vec![0u8; self.peer_metadata_size as usize];

        for piece in 0..num_pieces {
            self.request_metadata_piece(piece)?;
            let (received_piece, data) = self.read_metadata_piece()?;

            if received_piece != piece {
                return Err(anyhow!("received wrong metadata piece"));
            }

            let offset = (piece * METADATA_PIECE_SIZE) as usize;
            let end = std::cmp::min(offset + data.len(), metadata.len());
            metadata[offset..end].copy_from_slice(&data[..end - offset]);
        }

        info!("Downloaded complete metadata ({} bytes)", metadata.len());
        Ok(metadata)
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
        let handshake_len = buf[0] as usize;
        if handshake_len == 0 {
            return Err(anyhow!("invalid handshake length received from peer"));
        }

        Ok(handshake_len)
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
        let message_len = cursor.read_u32::<BigEndian>()? as usize;

        Ok(message_len)
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
        info!("Waiting for MESSAGE_BITFIELD from peer {:?}", self.peer.id);

        // Peers may send extension handshake or other messages before bitfield
        // Keep reading until we get the bitfield
        const MAX_MESSAGES: u32 = 100;
        for _ in 0..MAX_MESSAGES {
            let message: Message = self.read_message()?;

            match message.id {
                MESSAGE_BITFIELD => {
                    // Update bitfield
                    self.bitfield = message.payload.to_vec();
                    info!("Receive MESSAGE_BITFIELD from peer {:?}", self.peer.id);
                    return Ok(());
                }
                MESSAGE_EXTENDED => {
                    // Skip extension messages (handshake, etc.) - we don't need them for regular downloads
                    continue;
                }
                MESSAGE_CHOKE => {
                    self.read_choke();
                    continue;
                }
                MESSAGE_UNCHOKE => {
                    self.read_unchoke();
                    continue;
                }
                MESSAGE_HAVE => {
                    let _ = self.read_have(message);
                    continue;
                }
                MESSAGE_KEEPALIVE => {
                    continue;
                }
                _ => {
                    // Ignore other messages and keep waiting for bitfield
                    continue;
                }
            }
        }

        Err(anyhow!(
            "no bitfield received after {} messages",
            MAX_MESSAGES
        ))
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
            .set_read_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))
            .is_err()
        {
            return Err(anyhow!("could not set read timeout on new connection"));
        }

        if new_conn
            .set_write_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)))
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
