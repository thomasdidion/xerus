//! # BitTorrent Torrent Management
//!
//! This module handles torrent file parsing, tracker communication, and download
//! coordination. It implements the core BitTorrent protocol logic for managing
//! the download process from torrent metadata to completed files.
//!
//! ## Torrent File Format
//!
//! Torrent files contain metadata in bencoded format:
//!
//! - **announce**: Tracker URL for peer discovery
//! - **info**: Dictionary with file information and piece hashes
//! - **pieces**: Concatenated SHA-1 hashes for integrity verification
//! - **piece length**: Size of each piece (typically 256KB-1MB)
//! - **length**: Total file size
//! - **name**: Suggested filename
//!
//! ## Download Coordination
//!
//! The Torrent struct coordinates the entire download process:
//!
//! 1. **Parse torrent file** and extract metadata
//! 2. **Contact tracker** to discover peers
//! 3. **Create worker threads** for each peer
//! 4. **Distribute piece work** via channels
//! 5. **Collect results** and assemble the final file
//! 6. **Progress tracking** with visual progress bar
//!
//! ## Multi-threading Architecture
//!
//! - **Main thread**: Coordinates overall download process
//! - **Worker threads**: One per peer, handle piece downloads
//! - **Channels**: Crossbeam channels for work distribution and result collection
//! - **Progress bar**: Indicatif progress bar for user feedback

use crate::peer::*;
use crate::piece::*;
use crate::worker::*;

use anyhow::{anyhow, Result};
use boring::sha::Sha1;
use crossbeam_channel::{unbounded, Receiver, Sender};
use indicatif::{ProgressBar, ProgressStyle};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_bencode::{de, ser};
use serde_bytes::ByteBuf;
use std::str;
use url::Url;

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Default port for BitTorrent client connections
const PORT: u16 = 6881;
// Size of SHA-1 hash in bytes
const SHA1_HASH_SIZE: usize = 20;

/// Represents a BitTorrent torrent and manages the download process.
///
/// Contains all metadata from the torrent file and coordinates the download
/// from peer discovery through file assembly.
#[derive(Default, Clone)]
pub struct Torrent {
    /// Tracker tiers for peer discovery (each tier is a list of URLs)
    tiers: Vec<Vec<String>>,
    /// 20-byte SHA-1 hash of the bencoded info dictionary
    info_hash: Vec<u8>,
    /// Vector of 20-byte SHA-1 hashes, one for each piece
    pieces_hashes: Vec<Vec<u8>>,
    /// Size of each piece in bytes (except possibly the last)
    piece_length: u32,
    /// Total size of the file in bytes
    length: u32,
    /// Suggested filename from torrent metadata
    name: String,
    /// 20-byte unique identifier for this client instance
    peer_id: Vec<u8>,
    /// List of discovered peers available for downloading
    peers: Vec<Peer>,
}

/// BencodeInfo structure.
#[derive(Deserialize, Serialize)]
struct BencodeInfo {
    // Concatenation of all pieces 20-byte SHA-1 hashes
    #[serde(rename = "pieces")]
    pieces: ByteBuf,
    // Size of each piece in bytes
    #[serde(rename = "piece length")]
    piece_length: u32,
    // Size of the file in bytes
    #[serde(rename = "length")]
    length: u32,
    // Suggested filename where to save the file
    #[serde(rename = "name")]
    name: String,
}

/// BencodeTorrent structure.
#[derive(Deserialize, Serialize)]
struct BencodeTorrent {
    #[serde(default)]
    // URL of the tracker
    announce: String,
    #[serde(rename = "announce-list", default)]
    // List of tracker URLs
    announce_list: Vec<Vec<String>>,
    // Informations about file
    info: BencodeInfo,
}

/// BencodeTracker structure.
#[derive(Debug, Deserialize, Serialize)]
struct BencodeTracker {
    // Interval time to refresh the list of peers in seconds
    interval: u32,
    // Peers IP addresses
    peers: ByteBuf,
}

impl BencodeInfo {
    /// Hash bencoded informations to uniquely identify a file.
    fn hash(&self) -> Result<Vec<u8>> {
        // Serialize bencoded informations
        let buf: Vec<u8> = ser::to_bytes::<BencodeInfo>(self)?;

        // Hash bencoded informations
        let mut hasher = Sha1::new();
        hasher.update(&buf);

        // Read hash digest
        let hash = hasher.finish().to_vec();

        Ok(hash)
    }

    /// Split bencoded pieces into vectors of SHA-1 hashes.
    fn split_pieces_hashes(&self) -> Result<Vec<Vec<u8>>> {
        let pieces = self.pieces.to_owned();
        let nb_pieces = pieces.len();

        // Check torrent pieces
        if !nb_pieces.is_multiple_of(SHA1_HASH_SIZE) {
            return Err(anyhow!("torrent is invalid"));
        }
        let nb_hashes = nb_pieces / SHA1_HASH_SIZE;
        let mut hashes: Vec<Vec<u8>> = vec![vec![0; 20]; nb_hashes];

        // Split pieces
        for i in 0..nb_hashes {
            hashes[i] = pieces[i * SHA1_HASH_SIZE..(i + 1) * SHA1_HASH_SIZE].to_vec();
        }

        Ok(hashes)
    }
}

impl Torrent {
    /// Build a new torrent.
    pub fn new() -> Self {
        Default::default()
    }

    /// Returns the suggested filename from the torrent metadata.
    ///
    /// This is the filename specified in the torrent's "name" field,
    /// which should be used as the default output filename.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Open torrent.
    ///
    /// # Arguments
    ///
    /// * `filepath` - Path to the torrent.
    ///
    pub fn open(&mut self, filepath: PathBuf) -> Result<()> {
        // Open torrent
        let mut file = match File::open(filepath) {
            Ok(file) => file,
            Err(_) => return Err(anyhow!("could not open torrent")),
        };

        // Read torrent content in a buffer
        let mut buf = vec![];
        if file.read_to_end(&mut buf).is_err() {
            return Err(anyhow!("could not read torrent"));
        }
        // Deserialize bencoded data from torrent
        let bencode = match de::from_bytes::<BencodeTorrent>(&buf) {
            Ok(bencode) => bencode,
            Err(_) => return Err(anyhow!("could not decode torrent")),
        };

        // Generate a random 20-byte peer id
        let mut peer_id: Vec<u8> = vec![0; 20];
        let mut rng = rand::thread_rng();
        for x in peer_id.iter_mut() {
            *x = rng.gen();
        }

        // Add torrent informations
        if !bencode.announce_list.is_empty() {
            // Use announce-list, shuffle each tier as per BEP 12
            self.tiers = bencode.announce_list.clone();
            for tier in &mut self.tiers {
                let mut rng = rand::thread_rng();
                tier.shuffle(&mut rng);
            }
        } else if !bencode.announce.is_empty() {
            self.tiers = vec![vec![bencode.announce.to_owned()]];
        } else {
            return Err(anyhow!("torrent has no announce or announce-list"));
        }
        self.info_hash = bencode.info.hash()?;
        self.pieces_hashes = bencode.info.split_pieces_hashes()?;
        self.piece_length = bencode.info.piece_length;
        self.length = bencode.info.length;
        self.name = bencode.info.name.to_owned();
        self.peer_id = peer_id.clone();
        self.peers = self.request_peers(peer_id, PORT)?;

        Ok(())
    }

    /// Request peers from trackers.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - Urlencoded 20-byte string used as a unique ID for the client.
    /// * `port` - Port number that the client is listening on.
    ///
    fn request_peers(&self, peer_id: Vec<u8>, port: u16) -> Result<Vec<Peer>> {
        // Flatten all tiers into a unique list of tracker URLs
        let mut unique_urls = HashSet::new();
        for tier in &self.tiers {
            for url in tier {
                unique_urls.insert(url.clone());
            }
        }
        let tracker_urls: Vec<String> = unique_urls.into_iter().collect();

        if tracker_urls.is_empty() {
            return Err(anyhow!("no tracker URLs available"));
        }

        // Shared storage for peers bytes from successful tracker responses
        let all_peers_bytes = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        // Query all trackers in parallel
        for tracker_url in tracker_urls {
            let peer_id_clone = peer_id.clone();
            let info_hash_clone = self.info_hash.clone();
            let length_clone = self.length;
            let all_peers_bytes_clone = Arc::clone(&all_peers_bytes);

            let handle = thread::spawn(move || {
                // Build tracker URL
                let full_url = match Torrent::build_tracker_url(
                    &info_hash_clone,
                    &tracker_url,
                    peer_id_clone,
                    port,
                    length_clone,
                ) {
                    Ok(url) => url,
                    Err(_) => return, // skip on error
                };

                // Build blocking HTTP client
                let client = match reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(15))
                    .build()
                {
                    Ok(client) => client,
                    Err(_) => return, // skip on error
                };

                // Send GET request to the tracker
                let response = match client.get(&full_url).send() {
                    Ok(response) => match response.bytes() {
                        Ok(bytes) => bytes,
                        Err(_) => return, // skip on error
                    },
                    Err(_) => return, // skip on error
                };

                // Deserialize bencoded tracker response
                let tracker_bencode = match de::from_bytes::<BencodeTracker>(&response) {
                    Ok(bencode) => bencode,
                    Err(_) => return, // skip on error
                };

                // Store the peers bytes
                if let Ok(mut guard) = all_peers_bytes_clone.lock() {
                    guard.push(tracker_bencode.peers.to_vec());
                }
            });

            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            let _ = handle.join();
        }

        // Collect all peers from the responses
        let all_peers_bytes = all_peers_bytes.lock().unwrap();
        let mut all_peers = Vec::new();
        for peers_bytes in all_peers_bytes.iter() {
            match self.build_peers(peers_bytes.clone()) {
                Ok(mut peers) => all_peers.append(&mut peers),
                Err(_) => continue, // skip invalid peers
            }
        }

        if all_peers.is_empty() {
            return Err(anyhow!("could not get peers from any tracker"));
        }

        // Deduplicate peers by (ip, port)
        let mut unique_peers = HashSet::new();
        let mut deduped_peers = Vec::new();
        for peer in all_peers {
            if unique_peers.insert((peer.ip, peer.port)) {
                deduped_peers.push(peer);
            }
        }

        // Assign sequential IDs
        for (i, peer) in deduped_peers.iter_mut().enumerate() {
            peer.id = i as u32;
        }

        Ok(deduped_peers)
    }

    /// Build tracker URL.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - The 20-byte SHA-1 hash of the info dictionary.
    /// * `announce` - The tracker URL.
    /// * `peer_id` - Urlencoded 20-byte string used as a unique ID for the client.
    /// * `port` - Port number that the client is listening on.
    /// * `length` - Total file size in bytes.
    ///
    fn build_tracker_url(
        info_hash: &[u8],
        announce: &str,
        peer_id: Vec<u8>,
        port: u16,
        length: u32,
    ) -> Result<String> {
        /// Each byte is encoded as %XX where XX is the hexadecimal representation
        fn percent_encode_binary(data: &[u8]) -> String {
            const HEX_DIGITS: &[u8] = b"0123456789ABCDEF";
            let mut encoded = String::with_capacity(data.len() * 3);

            for &byte in data {
                encoded.push('%');
                // Extract high nibble (first 4 bits) and convert to hex digit
                encoded.push(HEX_DIGITS[(byte >> 4) as usize] as char);
                // Extract low nibble (last 4 bits) and convert to hex digit
                encoded.push(HEX_DIGITS[(byte & 0x0F) as usize] as char);
            }

            encoded
        }

        // Parse tracker URL from torrent
        let base_url = match Url::parse(announce) {
            Ok(url) => url,
            Err(_) => return Err(anyhow!("could not parse tracker url")),
        };

        // Build query string manually to handle binary data properly
        let query = format!(
            "info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1&event=started",
            percent_encode_binary(info_hash),
            percent_encode_binary(&peer_id),
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

    /// Download torrent.
    pub fn download(&self) -> Result<Vec<u8>> {
        println!(
            "Downloading {:?} ({:?} pieces)",
            self.name,
            self.pieces_hashes.len(),
        );

        // Create work pieces channel
        let work_chan: (Sender<PieceWork>, Receiver<PieceWork>) = unbounded();

        // Create result pieces channel
        let result_chan: (Sender<PieceResult>, Receiver<PieceResult>) = unbounded();

        // Create and send pieces to work channel
        for index in 0..self.pieces_hashes.len() {
            // Create piece
            let piece_index = index as u32;
            let piece_hash = self.pieces_hashes[index].clone();
            let piece_length = self.get_piece_length(piece_index)?;
            let piece_work = PieceWork::new(piece_index, piece_hash, piece_length);

            // Send piece to work channel
            if work_chan.0.send(piece_work).is_err() {
                return Err(anyhow!("Error: could not send piece to channel"));
            }
        }

        // Init workers
        let peers = self.peers.to_owned();
        for peer in peers {
            let peer_copy = peer.clone();
            let peer_id_copy = self.peer_id.clone();
            let info_hash_copy = self.info_hash.clone();
            let work_chan_copy = work_chan.clone();
            let result_chan_copy = result_chan.clone();

            // Create new worker
            let worker = Worker::new(
                peer_copy,
                peer_id_copy,
                info_hash_copy,
                work_chan_copy,
                result_chan_copy,
            )?;

            // Start worker in a new thread
            thread::spawn(move || {
                worker.start_download();
            });
        }

        // Create progress bar
        let pb = ProgressBar::new(self.length as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} {bytes}/{total_bytes} [{bar:40.cyan/blue}] {percent}%")
                .unwrap()
                .progress_chars("#>-"),
        );

        // Build torrent
        let mut data: Vec<u8> = vec![0; self.length as usize];
        let mut nb_pieces_downloaded = 0;
        while nb_pieces_downloaded < self.pieces_hashes.len() {
            // Receive a piece from result channel
            let piece_result: PieceResult = match result_chan.1.recv() {
                Ok(piece_result) => piece_result,
                Err(_) => return Err(anyhow!("Error: could not receive piece from channel")),
            };

            // Copy piece data
            let begin: u32 = self.get_piece_offset(piece_result.index)?;
            for i in 0..piece_result.length as usize {
                data[begin as usize + i] = piece_result.data[i];
            }

            // Update progress bar
            pb.inc(piece_result.length as u64);

            // Update number of pieces downloaded
            nb_pieces_downloaded += 1;
        }

        Ok(data)
    }

    /// Get piece length.
    ///
    /// # Arguments
    ///
    /// * `index` - The piece index.
    ///
    fn get_piece_length(&self, index: u32) -> Result<u32> {
        let begin: u32 = index * self.piece_length;
        let mut end: u32 = begin + self.piece_length;

        // Prevent unbounded values
        if end > self.length {
            end = self.length;
        }

        Ok(end - begin)
    }

    /// Get piece offset.
    ///
    /// # Arguments
    ///
    /// * `index` - The piece index.
    ///
    fn get_piece_offset(&self, index: u32) -> Result<u32> {
        let mut offset: u32 = 0;

        // Calculate the actual offset by summing up the lengths of all previous pieces
        for i in 0..index {
            offset += self.get_piece_length(i)?;
        }

        Ok(offset)
    }
}
