//! # Magnet Link Support
//!
//! This module handles magnet link parsing and metadata fetching from peers.
//! It implements BEP 9 (Extension for Peers to Send Metadata Files) and
//! BEP 10 (Extension Protocol) for fetching torrent metadata without a .torrent file.
//!
//! ## Magnet Link Format
//!
//! ```text
//! magnet:?xt=urn:btih:<info_hash>&dn=<name>&tr=<tracker_url>
//! ```
//!
//! ## Metadata Fetching
//!
//! When using magnet links, the torrent metadata (info dictionary) must be
//! fetched from peers using the ut_metadata extension:
//!
//! 1. **Connect to peers** discovered via trackers
//! 2. **Extension handshake** to negotiate ut_metadata support
//! 3. **Request metadata pieces** from supporting peers
//! 4. **Verify metadata** against the info_hash from the magnet link

use crate::client::*;
use crate::peer::*;

use anyhow::{anyhow, Result};
use crossbeam_channel::unbounded;
use url::Url;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

// Maximum number of concurrent peer connections for metadata fetching
const MAX_METADATA_PEERS: usize = 20;

/// Parsed magnet link parameters.
pub struct MagnetInfo {
    /// 20-byte SHA-1 hash of the bencoded info dictionary
    pub info_hash: Vec<u8>,
    /// Suggested filename from magnet link
    pub name: String,
    /// Tracker tiers for peer discovery
    pub tiers: Vec<Vec<String>>,
}

/// Parse a magnet URI.
///
/// Extracts the info_hash, display name and tracker URLs from the magnet link.
///
/// # Arguments
///
/// * `uri` - Magnet URI string.
///
pub fn parse_magnet(uri: &str) -> Result<MagnetInfo> {
    let url = Url::parse(uri).map_err(|_| anyhow!("invalid magnet link"))?;

    if url.scheme() != "magnet" {
        return Err(anyhow!("not a magnet link"));
    }

    let mut info_hash = Vec::new();
    let mut name = String::new();
    let mut tiers = Vec::new();

    // Parse magnet parameters
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" if value.starts_with("urn:btih:") => {
                let hash = &value[9..]; // Skip "urn:btih:"
                info_hash = hex::decode(hash).map_err(|_| anyhow!("invalid info_hash"))?;
            }
            "dn" => name = value.into_owned(),
            "tr" => tiers.push(vec![value.into_owned()]),
            _ => {}
        }
    }

    if info_hash.is_empty() {
        return Err(anyhow!("magnet link missing info_hash"));
    }
    if tiers.is_empty() {
        return Err(anyhow!("magnet link missing tracker"));
    }

    Ok(MagnetInfo {
        info_hash,
        name,
        tiers,
    })
}

/// Fetch torrent metadata from peers using ut_metadata extension (BEP 9).
///
/// Connects to multiple peers in parallel and returns metadata from the first
/// peer that successfully provides it.
///
/// # Arguments
///
/// * `peers` - List of discovered peers to try.
/// * `peer_id` - 20-byte unique identifier for this client instance.
/// * `info_hash` - 20-byte SHA-1 hash of the bencoded info dictionary.
///
pub fn fetch_metadata_from_peers(
    peers: &[Peer],
    peer_id: &[u8],
    info_hash: &[u8],
) -> Result<Vec<u8>> {
    println!("Fetching metadata from {} peers...", peers.len());

    let (tx, rx) = unbounded::<Vec<u8>>();
    let done = Arc::new(AtomicBool::new(false));

    // Spawn threads for peers in parallel
    for peer in peers.iter().take(MAX_METADATA_PEERS).cloned() {
        let tx = tx.clone();
        let done = Arc::clone(&done);
        let peer_id = peer_id.to_vec();
        let info_hash = info_hash.to_vec();

        thread::spawn(move || {
            let peer_addr = format!("{}:{}", peer.ip, peer.port);

            // Check if another thread already succeeded
            if done.load(Ordering::Relaxed) {
                return;
            }

            let Ok(mut client) = Client::new(peer.clone(), peer_id, info_hash) else {
                debug!("[{}] connection failed", peer_addr);
                return;
            };

            if done.load(Ordering::Relaxed) {
                return;
            }
            match client.handshake_with_peer() {
                Ok(_) if !client.supports_extensions() => {
                    debug!("[{}] no extension support", peer_addr);
                    return;
                }
                Err(e) => {
                    debug!("[{}] handshake failed: {}", peer_addr, e);
                    return;
                }
                Ok(_) => {}
            }

            if done.load(Ordering::Relaxed) || client.send_extension_handshake().is_err() {
                return;
            }

            if done.load(Ordering::Relaxed) {
                return;
            }
            match client.read_extension_handshake() {
                Err(e) => {
                    debug!("[{}] ext handshake failed: {}", peer_addr, e);
                    return;
                }
                Ok(_) if !client.supports_ut_metadata() => {
                    debug!("[{}] no ut_metadata support", peer_addr);
                    return;
                }
                Ok(_) => {}
            }

            if done.load(Ordering::Relaxed) {
                return;
            }
            match client.download_metadata() {
                Ok(metadata) => {
                    // Signal success and send metadata
                    done.store(true, Ordering::Relaxed);
                    let _ = tx.send(metadata);
                }
                Err(e) => {
                    debug!("[{}] metadata download failed: {}", peer_addr, e);
                }
            }
        });
    }

    // Drop our sender so rx.recv() will return when all threads finish
    drop(tx);

    // Wait for first successful result or all threads to fail
    match rx.recv() {
        Ok(metadata) => {
            println!("Metadata received ({} bytes)", metadata.len());
            Ok(metadata)
        }
        Err(_) => Err(anyhow!("could not fetch metadata from any peer")),
    }
}
