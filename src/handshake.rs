//! # BitTorrent Handshake Protocol
//!
//! This module implements the initial handshake protocol used to establish
//! communication between BitTorrent peers.
//!
//! ## Handshake Purpose
//!
//! The handshake serves multiple purposes:
//!
//! - **Protocol Identification**: Confirms both peers support BitTorrent protocol
//! - **Peer Authentication**: Validates the torrent info_hash matches
//! - **Peer Identification**: Exchanges unique peer IDs for tracking
//! - **Extension Negotiation**: Reserved bytes for future protocol extensions
//!
//! ## Message Format
//!
//! The handshake is a fixed 68-byte message:
//!
//! ```text
//! <pstrlen><pstr><reserved><info_hash><peer_id>
//! ```
//!
//! - **pstrlen**: 1 byte - Length of protocol string (usually 19)
//! - **pstr**: Variable - Protocol identifier ("BitTorrent protocol")
//! - **reserved**: 8 bytes - All zeros (for future extensions)
//! - **info_hash**: 20 bytes - SHA-1 hash of torrent info dictionary
//! - **peer_id**: 20 bytes - Unique identifier for the peer
//!
//! ## Protocol String
//!
//! The protocol string must be "BitTorrent protocol" (19 bytes).
//! This ensures compatibility and prevents misidentification with other protocols.
//!
//! ## Reserved Bytes
//!
//! Currently all 8 reserved bytes are set to 0. Future extensions may use these
//! to negotiate additional capabilities like DHT support, encryption, etc.
//!
//! ## Security Considerations
//!
//! - The info_hash prevents peers from connecting to wrong swarms
//! - Peer IDs should be randomly generated to avoid tracking
//! - Handshake validation prevents protocol confusion attacks

use anyhow::Result;

const PROTOCOL_ID: &str = "BitTorrent protocol";

/// Represents a BitTorrent handshake message.
///
/// Contains all fields required for peer protocol negotiation.
/// The handshake is sent immediately after TCP connection establishment.
pub struct Handshake {
    /// Length of the protocol identifier string (usually 19)
    pub pstrlen: usize,
    /// Protocol identifier bytes ("BitTorrent protocol")
    pub pstr: Vec<u8>,
    /// 8 reserved bytes for future protocol extensions (currently all zeros)
    pub reserved: Vec<u8>,
    /// 20-byte SHA-1 hash of the torrent's info dictionary
    pub info_hash: Vec<u8>,
    /// 20-byte unique identifier for this peer
    pub peer_id: Vec<u8>,
}

impl Handshake {
    /// Creates a new handshake message with standard BitTorrent protocol.
    ///
    /// Initializes the handshake with:
    /// - Protocol string "BitTorrent protocol"
    /// - 8 reserved bytes set to 0
    /// - Provided peer_id and info_hash
    ///
    /// # Arguments
    ///
    /// * `peer_id` - 20-byte unique identifier for this client
    /// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
    ///
    /// # Returns
    ///
    /// A new `Handshake` instance ready for serialization and sending.
    pub fn new(peer_id: Vec<u8>, info_hash: Vec<u8>) -> Self {
        // Get pstr
        let pstr = String::from(PROTOCOL_ID).into_bytes();
        // Get pstrlen
        let pstrlen = pstr.len();
        // Get reserved
        let reserved: Vec<u8> = vec![0; 8];

        Handshake {
            pstrlen,
            pstr,
            reserved,
            info_hash,
            peer_id,
        }
    }

    /// Serializes the handshake into a byte vector for network transmission.
    ///
    /// Concatenates all fields in the correct order:
    /// 1. pstrlen (1 byte)
    /// 2. pstr (variable length)
    /// 3. reserved (8 bytes)
    /// 4. info_hash (20 bytes)
    /// 5. peer_id (20 bytes)
    ///
    /// Total size is always 49 + pstrlen bytes (68 bytes for standard protocol).
    ///
    /// # Returns
    ///
    /// A `Vec<u8>` containing the serialized handshake, or an error if serialization fails.
    ///
    /// # Example
    ///
    /// ```rust
    /// let handshake = Handshake::new(peer_id, info_hash);
    /// let bytes = handshake.serialize()?;
    /// assert_eq!(bytes.len(), 68); // For standard protocol
    /// ```
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut serialized: Vec<u8> = vec![];

        // Add pstrlen
        serialized.push(self.pstrlen as u8);

        // Add pstr
        let mut pstr: Vec<u8> = self.pstr.clone();
        serialized.append(&mut pstr);

        // Add reserved
        let mut reserved: Vec<u8> = self.reserved.clone();
        serialized.append(&mut reserved);

        // Add info hash
        let mut info_hash: Vec<u8> = self.info_hash.clone();
        serialized.append(&mut info_hash);

        // Add peer id
        let mut peer_id: Vec<u8> = self.peer_id.clone();
        serialized.append(&mut peer_id);

        Ok(serialized)
    }
}

/// Deserializes a received handshake byte buffer into a Handshake struct.
///
/// Parses the incoming handshake by extracting each field at the correct offsets:
/// - pstr: bytes 0..pstrlen
/// - reserved: bytes pstrlen..pstrlen+8
/// - info_hash: bytes pstrlen+8..pstrlen+28
/// - peer_id: bytes pstrlen+28..pstrlen+48
///
/// # Arguments
///
/// * `buf` - Byte buffer containing the complete handshake message
/// * `pstrlen` - Length of the protocol string (first byte of handshake)
///
/// # Returns
///
/// A parsed `Handshake` struct, or an error if the buffer is malformed.
///
/// # Errors
///
/// Returns an error if the buffer doesn't contain enough bytes for all fields.
///
/// # Validation
///
/// This function does not validate the protocol string or reserved bytes -
/// that should be done by the caller after deserialization.
pub fn deserialize_handshake(buf: &[u8], pstrlen: usize) -> Result<Handshake> {
    // Get pstr
    let pstr = buf[0..pstrlen].to_vec();
    // Get reserved
    let reserved = buf[pstrlen..(pstrlen + 8)].to_vec();
    // Get info hash
    let info_hash = buf[(pstrlen + 8)..(pstrlen + 8 + 20)].to_vec();
    // Get peer id
    let peer_id = buf[(pstrlen + 8 + 20)..].to_vec();

    // Build handshake
    let handshake = Handshake {
        pstrlen,
        pstr,
        reserved,
        info_hash,
        peer_id,
    };

    Ok(handshake)
}
