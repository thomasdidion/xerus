//! # BitTorrent Tracker Communication
//!
//! This module handles communication with BitTorrent trackers for peer discovery.
//! It supports both HTTP and UDP tracker protocols.
//!
//! ## Supported Protocols
//!
//! - **HTTP/HTTPS trackers**: Standard HTTP GET with query parameters
//! - **UDP trackers**: Binary protocol with connect and announce phases
//!
//! ## Tracker Flow
//!
//! 1. **Query all tracker tiers** for peers
//! 2. **Deduplicate peers** by IP:port
//! 3. **Assign sequential IDs** to peers
//!
//! ## UDP Tracker Protocol
//!
//! UDP trackers use a two-phase protocol:
//!
//! 1. **Connect**: Send protocol magic + transaction ID, receive connection ID
//! 2. **Announce**: Send connection ID + torrent info, receive peer list

use crate::peer::{self, Peer};

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use serde::{Deserialize, Serialize};
use serde_bencode::de;
use serde_bytes::ByteBuf;
use url::Url;

use std::collections::HashSet;
use std::io::Read;
use std::net::UdpSocket;

// Default "left" value when total length is unknown (1 GiB)
const DEFAULT_LEFT: u64 = 1024 * 1024 * 1024;
// Maximum number of peers to accept across all trackers
const MAX_PEERS: usize = 100;
// Maximum HTTP tracker response size (1 MiB)
const MAX_HTTP_RESPONSE_SIZE: usize = 1024 * 1024;
// UDP tracker protocol magic number (connection ID for initial connect request)
const UDP_TRACKER_MAGIC: u64 = 0x41727101980;
// UDP tracker socket timeout in seconds
const UDP_TRACKER_TIMEOUT_SECS: u64 = 5;
// Default UDP tracker port
const UDP_TRACKER_DEFAULT_PORT: u16 = 6969;

/// BencodeTracker structure.
#[derive(Debug, Deserialize, Serialize)]
struct BencodeTracker {
    // Interval time to refresh the list of peers in seconds
    interval: u32,
    // Peers IP addresses
    peers: ByteBuf,
}

/// Percent-encode binary data for URL query strings.
///
/// Each byte is encoded as `%XX` where XX is the uppercase hexadecimal
/// representation. Required for encoding info_hash and peer_id in tracker URLs.
///
/// # Arguments
///
/// * `data` - The binary data to encode.
///
fn percent_encode_binary(data: &[u8]) -> String {
    const HEX_DIGITS: &[u8] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(data.len() * 3);
    for &byte in data {
        encoded.push('%');
        encoded.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        encoded.push(HEX_DIGITS[(byte & 0x0F) as usize] as char);
    }
    encoded
}

/// Request peers from all trackers.
///
/// Queries all tracker tiers, combines and deduplicates peers by IP:port,
/// and assigns sequential IDs.
///
/// # Arguments
///
/// * `tiers` - Tracker tiers for peer discovery (each tier is a list of URLs).
/// * `info_hash` - 20-byte SHA-1 hash of the bencoded info dictionary.
/// * `peer_id` - 20-byte unique identifier for this client instance.
/// * `port` - Port number for incoming peer connections.
/// * `length` - Total size of all files in bytes.
///
pub fn request_peers(
    tiers: &[Vec<String>],
    info_hash: &[u8],
    peer_id: &[u8],
    port: u16,
    length: u64,
) -> Result<Vec<Peer>> {
    let mut all_peers = Vec::new();
    let mut seen = HashSet::new();

    for tier in tiers {
        for url in tier {
            match query_single_tracker(url, info_hash, peer_id, port, length) {
                Ok(peers) => {
                    let new_count = peers
                        .into_iter()
                        .filter(|peer| seen.insert((peer.ip, peer.port)))
                        .take(MAX_PEERS - all_peers.len())
                        .map(|peer| all_peers.push(peer))
                        .count();
                    debug!(
                        "Tracker {}: {} new peers (total: {})",
                        url,
                        new_count,
                        all_peers.len()
                    );
                }
                Err(e) => debug!("Tracker {}: {}", url, e),
            }

            // Stop querying trackers if we have enough peers
            if all_peers.len() >= MAX_PEERS {
                break;
            }
        }
    }

    if all_peers.is_empty() {
        return Err(anyhow!("no peers from any tracker"));
    }

    for (i, peer) in all_peers.iter_mut().enumerate() {
        peer.id = i as u32;
    }
    Ok(all_peers)
}

/// Query a single tracker for peers.
///
/// Supports HTTP, HTTPS and UDP trackers.
///
/// # Arguments
///
/// * `announce` - The tracker announce URL.
/// * `info_hash` - 20-byte SHA-1 hash of the bencoded info dictionary.
/// * `peer_id` - 20-byte unique identifier for this client instance.
/// * `port` - Port number for incoming peer connections.
/// * `length` - Total size of all files in bytes.
///
fn query_single_tracker(
    announce: &str,
    info_hash: &[u8],
    peer_id: &[u8],
    port: u16,
    length: u64,
) -> Result<Vec<Peer>> {
    if announce.starts_with("udp://") {
        query_udp_tracker(announce, info_hash, peer_id, port, length)
    } else if announce.starts_with("http://") || announce.starts_with("https://") {
        let url = build_tracker_url(info_hash, announce, peer_id, port, length)?;
        let response = reqwest::blocking::get(&url)?;
        let bytes = response.bytes()?;
        if bytes.len() > MAX_HTTP_RESPONSE_SIZE {
            return Err(anyhow!("HTTP tracker response too large"));
        }
        let resp = de::from_bytes::<BencodeTracker>(&bytes)?;
        peer::parse_peers(resp.peers.to_vec())
    } else {
        // Skip unsupported protocols
        Err(anyhow!("unsupported tracker protocol"))
    }
}

/// Query a UDP tracker for peers.
///
/// Implements the UDP tracker protocol with connect and announce phases.
///
/// # Arguments
///
/// * `announce` - The UDP tracker announce URL.
/// * `info_hash` - 20-byte SHA-1 hash of the bencoded info dictionary.
/// * `peer_id` - 20-byte unique identifier for this client instance.
/// * `port` - Port number for incoming peer connections.
/// * `length` - Total size of all files in bytes.
///
fn query_udp_tracker(
    announce: &str,
    info_hash: &[u8],
    peer_id: &[u8],
    port: u16,
    length: u64,
) -> Result<Vec<Peer>> {
    let url = Url::parse(announce).map_err(|_| anyhow!("could not parse UDP tracker url"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("invalid UDP tracker host"))?;
    // UDP trackers typically use port 6969, but use port from URL if specified
    let port_num = url.port().unwrap_or(UDP_TRACKER_DEFAULT_PORT);

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect((host, port_num))?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(
        UDP_TRACKER_TIMEOUT_SECS,
    )))?;

    // Send connect request
    let mut connect_req = Vec::new();
    connect_req.write_u64::<BigEndian>(UDP_TRACKER_MAGIC)?;
    connect_req.write_u32::<BigEndian>(0)?; // action (connect)
    let transaction_id = rand::random::<u32>();
    connect_req.write_u32::<BigEndian>(transaction_id)?;

    socket.send(&connect_req)?;

    // Receive connect response
    let mut buf = [0u8; 16];
    if socket.recv(&mut buf).is_err() {
        return Err(anyhow!(
            "UDP tracker timeout (no response within {} seconds)",
            UDP_TRACKER_TIMEOUT_SECS
        ));
    }
    let mut reader = std::io::Cursor::new(&buf);
    let action = reader.read_u32::<BigEndian>()?;
    let resp_transaction_id = reader.read_u32::<BigEndian>()?;
    if action != 0 {
        return Err(anyhow!("UDP tracker connect failed: action {}", action));
    }
    if resp_transaction_id != transaction_id {
        return Err(anyhow!("UDP tracker transaction ID mismatch"));
    }
    let connection_id = reader.read_u64::<BigEndian>()?;

    // Send announce request
    let mut ann_req = Vec::new();
    ann_req.write_u64::<BigEndian>(connection_id)?;
    ann_req.write_u32::<BigEndian>(1)?; // action (announce)
    ann_req.write_u32::<BigEndian>(transaction_id)?;
    ann_req.extend_from_slice(info_hash);
    ann_req.extend_from_slice(peer_id);
    ann_req.write_u64::<BigEndian>(0)?;
    // Use dummy left if total length is unknown
    let left = if length == 0 { DEFAULT_LEFT } else { length };
    ann_req.write_u64::<BigEndian>(left)?;
    ann_req.write_u64::<BigEndian>(0)?; // uploaded
    ann_req.write_u32::<BigEndian>(0)?; // event (none)
    ann_req.write_u32::<BigEndian>(0)?; // IP (0 = default)
    ann_req.write_u32::<BigEndian>(rand::random::<u32>())?; // key
    ann_req.write_i32::<BigEndian>(-1)?; // num_want (-1 = default)
    ann_req.write_u16::<BigEndian>(port)?;

    socket.send(&ann_req)?;

    // Receive announce response
    let mut buf = vec![0u8; 1024];
    let len = match socket.recv(&mut buf) {
        Ok(len) => len,
        Err(_) => {
            return Err(anyhow!(
                "UDP tracker announce timeout (no response within {} seconds)",
                UDP_TRACKER_TIMEOUT_SECS
            ));
        }
    };
    buf.truncate(len);
    let mut reader = std::io::Cursor::new(&buf);

    let action = reader.read_u32::<BigEndian>()?;
    if action == 3 {
        // Error response
        let error_msg_len = len - 8;
        let error_msg = String::from_utf8_lossy(&buf[8..8 + error_msg_len]);
        return Err(anyhow!("UDP tracker error: {}", error_msg));
    }
    if action != 1 {
        return Err(anyhow!("UDP tracker announce failed: action {}", action));
    }

    let _transaction_id = reader.read_u32::<BigEndian>()?;
    let _interval = reader.read_u32::<BigEndian>()?;
    let _leechers = reader.read_u32::<BigEndian>()?;
    let _seeders = reader.read_u32::<BigEndian>()?;

    // Read peers (6 bytes each: 4 bytes IP + 2 bytes port)
    let mut peers_buf = Vec::new();
    reader.read_to_end(&mut peers_buf)?;
    peer::parse_peers(peers_buf)
}

/// Build tracker announce URL with required query parameters.
///
/// Constructs the full URL with percent-encoded info_hash and peer_id,
/// plus standard tracker parameters (port, uploaded, downloaded, left, compact).
///
/// # Arguments
///
/// * `info_hash` - 20-byte SHA-1 hash of the bencoded info dictionary.
/// * `announce` - The tracker announce URL.
/// * `peer_id` - 20-byte unique identifier for this client instance.
/// * `port` - Port number for incoming peer connections.
/// * `length` - Total size of all files in bytes.
///
fn build_tracker_url(
    info_hash: &[u8],
    announce: &str,
    peer_id: &[u8],
    port: u16,
    length: u64,
) -> Result<String> {
    let base_url = match Url::parse(announce) {
        Ok(url) => url,
        Err(_) => return Err(anyhow!("could not parse tracker url")),
    };

    // Build query string manually to handle binary data properly
    let query = format!(
        "info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1&event=started",
        percent_encode_binary(info_hash),
        percent_encode_binary(peer_id),
        port,
        length
    );

    let mut url = base_url.to_string();
    if url.contains('?') {
        url.push('&');
    } else {
        url.push('?');
    }
    url.push_str(&query);

    Ok(url)
}
