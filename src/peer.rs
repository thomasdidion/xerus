//! # BitTorrent Peer Information
//!
//! This module defines the `Peer` structure and provides functionality for parsing
//! peer information received from BitTorrent trackers.
//!
//! ## Peer Discovery
//!
//! Peers are discovered through tracker communication. The tracker responds with a
//! compact binary format containing IP addresses and ports of available peers.
//!
//! ## Compact Peer Format
//!
//! The compact format consists of 6-byte entries:
//!
//! ```text
//! <IP: 4 bytes><Port: 2 bytes>
//! ```
//!
//! - IP address in network byte order (big-endian)
//! - Port number in network byte order (big-endian)
//!
//! ## Peer Structure
//!
//! Each peer contains:
//! - Unique ID for internal tracking
//! - IPv4 address
//! - Port number for connection

use crate::torrent::*;

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};

use std::io::Cursor;
use std::net::Ipv4Addr;

const PEER_SIZE: usize = 6;

type PeerId = u32;

/// Represents a BitTorrent peer in the swarm.
///
/// Contains the network information needed to connect to a peer and a unique
/// identifier for internal tracking purposes.
#[derive(Clone)]
pub struct Peer {
    /// Unique identifier assigned to this peer for internal tracking
    pub id: PeerId,
    /// IPv4 address of the peer
    pub ip: Ipv4Addr,
    /// Port number for connecting to the peer
    pub port: u16,
}

impl Peer {
    /// Creates a new peer with default/placeholder values.
    ///
    /// Used as a template when parsing peer lists from tracker responses.
    /// The actual values are filled in during parsing.
    pub fn new() -> Peer {
        Peer {
            id: 0,
            ip: Ipv4Addr::new(1, 1, 1, 1),
            port: 0,
        }
    }
}

impl Torrent {
    /// Parses a compact peer list from tracker response into Peer structures.
    ///
    /// Converts the binary peer list received from the tracker into a vector of
    /// Peer instances that can be used for connection establishment.
    ///
    /// # Arguments
    ///
    /// * `tracker_peers` - Compact binary peer list where each peer is 6 bytes:
    ///   - Bytes 0-3: IPv4 address (big-endian)
    ///   - Bytes 4-5: Port number (big-endian)
    ///
    /// # Returns
    ///
    /// Returns a `Result<Vec<Peer>>` containing all parsed peers, or an error if
    /// the peer list format is invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer list length is not a multiple of 6 bytes.
    ///
    /// # Example
    ///
    /// ```rust
    /// // Assuming tracker returned 12 bytes (2 peers)
    /// let peer_data = vec![192, 168, 1, 1, 0, 80, 192, 168, 1, 2, 0, 80];
    /// let peers = torrent.build_peers(peer_data)?;
    /// assert_eq!(peers.len(), 2);
    /// ```
    pub fn build_peers(&self, tracker_peers: Vec<u8>) -> Result<Vec<Peer>> {
        // Check tracker peers are valid
        if !tracker_peers.len().is_multiple_of(PEER_SIZE) {
            return Err(anyhow!("received invalid peers from tracker"));
        }

        // Get number of peers
        let nb_peers = tracker_peers.len() / PEER_SIZE;

        // Build peers
        let mut peers: Vec<Peer> = vec![Peer::new(); nb_peers];

        for (i, peer) in peers.iter_mut().enumerate().take(nb_peers) {
            // Create peer ID
            peer.id = i as u32;

            let offset = i * PEER_SIZE;

            // Read peer IP address
            peer.ip = Ipv4Addr::new(
                tracker_peers[offset],
                tracker_peers[offset + 1],
                tracker_peers[offset + 2],
                tracker_peers[offset + 3],
            );

            // Read peer port
            let port_bytes = &tracker_peers[offset + 4..offset + 6];
            let mut port_cursor = Cursor::new(port_bytes);
            peer.port = port_cursor.read_u16::<BigEndian>()?;
        }

        Ok(peers)
    }
}
