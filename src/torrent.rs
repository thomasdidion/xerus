//! # BitTorrent Torrent Management
//!
//! This module handles torrent file and magnet link parsing, tracker communication,
//! and download coordination. It implements the core BitTorrent protocol logic for
//! managing the download process from torrent metadata to completed files.
//!
//! ## Supported Inputs
//!
//! - **Torrent files**: Bencoded `.torrent` files with full metadata
//! - **Magnet links**: URI with info_hash, fetches metadata from peers (BEP 9, BEP 10)
//!
//! ## Download Coordination
//!
//! The Torrent struct coordinates the entire download process:
//!
//! 1. **Parse input** (torrent file or magnet link)
//! 2. **Contact tracker** to discover peers
//! 3. **Fetch metadata** from peers (magnet links only)
//! 4. **Create worker threads** for each peer
//! 5. **Distribute piece work** via channels
//! 6. **Collect results** and assemble the final file
//!
//! ## Multi-threading Architecture
//!
//! - **Main thread**: Coordinates overall download process
//! - **Worker threads**: One per peer, handle piece downloads
//! - **Channels**: Crossbeam channels for work distribution and result collection
//! - **Progress bar**: Indicatif progress bar for user feedback

use crate::magnet;
use crate::peer::*;
use crate::piece::*;
use crate::tracker;
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

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::thread;

// Default port for BitTorrent client connections
const PORT: u16 = 6881;
// Size of SHA-1 hash in bytes
const SHA1_HASH_SIZE: usize = 20;

/// Represents a BitTorrent torrent and manages the download process.
///
/// File entry for multi-file torrents.
#[derive(Default, Clone, Debug)]
pub struct FileInfo {
    /// Size of this file in bytes
    pub length: u64,
    /// Path components for this file (e.g., ["dir", "subdir", "file.txt"])
    pub path: Vec<String>,
}

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
    /// Total size of all files in bytes
    length: u64,
    /// Suggested filename from torrent metadata
    name: String,
    /// 20-byte unique identifier for this client instance
    peer_id: Vec<u8>,
    /// List of discovered peers available for downloading
    peers: Vec<Peer>,
    /// File list for multi-file torrents (empty for single-file)
    files: Vec<FileInfo>,
}

/// Single file entry in multi-file torrent.
#[derive(Deserialize, Serialize)]
struct BencodeFile {
    // Size of this file in bytes
    length: u64,
    // Path components for this file
    path: Vec<String>,
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
    // Size of the file in bytes (single-file mode)
    #[serde(rename = "length", default)]
    length: Option<u64>,
    // Files list (multi-file mode)
    #[serde(default)]
    files: Option<Vec<BencodeFile>>,
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

    /// Calculates total length from either single-file length or multi-file sum.
    fn total_length(&self) -> Result<u64> {
        if let Some(length) = self.length {
            Ok(length)
        } else if let Some(ref files) = self.files {
            Ok(files.iter().map(|f| f.length).sum())
        } else {
            Err(anyhow!("torrent has neither length nor files"))
        }
    }

    /// Returns file list for multi-file torrents, empty for single-file.
    fn file_list(&self) -> Vec<FileInfo> {
        match &self.files {
            Some(files) => files
                .iter()
                .map(|f| FileInfo {
                    length: f.length,
                    path: f.path.clone(),
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Split bencoded pieces into vectors of SHA-1 hashes.
    fn split_pieces_hashes(&self) -> Result<Vec<Vec<u8>>> {
        if !self.pieces.len().is_multiple_of(SHA1_HASH_SIZE) {
            return Err(anyhow!("torrent is invalid"));
        }

        Ok(self
            .pieces
            .chunks(SHA1_HASH_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect())
    }
}

impl Torrent {
    /// Build a new torrent.
    pub fn new() -> Self {
        Default::default()
    }

    /// Generates a random 20-byte peer ID.
    ///
    /// The peer ID uniquely identifies this client instance in the BitTorrent swarm.
    /// It is randomly generated for each session to prevent tracking.
    fn generate_peer_id() -> Vec<u8> {
        let mut peer_id = vec![0u8; 20];
        let mut rng = rand::rng();
        for x in peer_id.iter_mut() {
            *x = rng.random::<u8>();
        }
        peer_id
    }

    /// Returns the suggested filename from the torrent metadata.
    ///
    /// This is the filename specified in the torrent's "name" field,
    /// which should be used as the default output filename.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns true if this is a multi-file torrent.
    pub fn is_multi_file(&self) -> bool {
        !self.files.is_empty()
    }

    /// Returns the list of files for multi-file torrents.
    pub fn files(&self) -> &[FileInfo] {
        &self.files
    }

    /// Open torrent.
    ///
    /// # Arguments
    ///
    /// * `filepath` - Path to the .torrent file.
    ///
    pub fn open_torrent(&mut self, filepath: PathBuf) -> Result<()> {
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

        let peer_id = Self::generate_peer_id();

        // Add torrent informations
        if !bencode.announce_list.is_empty() {
            self.tiers = bencode.announce_list.clone();
            for tier in &mut self.tiers {
                tier.shuffle(&mut rand::rng());
            }
        } else if !bencode.announce.is_empty() {
            self.tiers = vec![vec![bencode.announce]];
        } else {
            return Err(anyhow!("torrent has no announce or announce-list"));
        }
        self.info_hash = bencode.info.hash()?;
        self.pieces_hashes = bencode.info.split_pieces_hashes()?;
        self.piece_length = bencode.info.piece_length;
        self.length = bencode.info.total_length()?;
        self.name = bencode.info.name.to_owned();
        self.files = bencode.info.file_list();
        self.peers =
            tracker::request_peers(&self.tiers, &self.info_hash, &peer_id, PORT, self.length)?;
        self.peer_id = peer_id;

        Ok(())
    }

    /// Open magnet.
    ///
    /// # Arguments
    ///
    /// * `uri` - Magnet URI string.
    ///
    pub fn open_magnet(&mut self, uri: &str) -> Result<()> {
        // Parse magnet link
        let magnet_info = magnet::parse_magnet(uri)?;
        self.info_hash = magnet_info.info_hash;
        self.name = magnet_info.name;
        self.tiers = magnet_info.tiers;

        let peer_id = Self::generate_peer_id();

        // Get peers from tracker
        self.peers =
            tracker::request_peers(&self.tiers, &self.info_hash, &peer_id, PORT, self.length)?;
        self.peer_id = peer_id.clone();

        // Fetch metadata from peers
        let metadata = magnet::fetch_metadata_from_peers(&self.peers, &peer_id, &self.info_hash)?;

        // Parse and verify metadata
        let info: BencodeInfo = de::from_bytes(&metadata)?;
        if info.hash()? != self.info_hash {
            return Err(anyhow!("metadata info_hash mismatch"));
        }

        // Populate torrent fields
        self.pieces_hashes = info.split_pieces_hashes()?;
        self.piece_length = info.piece_length;
        self.length = info.total_length()?;
        self.files = info.file_list();
        if self.name.is_empty() {
            self.name = info.name;
        }

        Ok(())
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
        for peer in &self.peers {
            let worker = Worker::new(
                peer.clone(),
                self.peer_id.clone(),
                self.info_hash.clone(),
                work_chan.clone(),
                result_chan.clone(),
            )?;

            thread::spawn(move || {
                worker.start_download();
            });
        }

        // Create progress bar
        let pb = ProgressBar::new(self.length);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} {bytes}/{total_bytes} [{bar:40.cyan/blue}] {percent}%")
                .expect("Failed to set progress style")
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
            let begin = self.get_piece_offset(piece_result.index) as usize;
            let end = begin + piece_result.length as usize;
            data[begin..end].copy_from_slice(&piece_result.data);

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
        let begin: u64 = u64::from(index) * u64::from(self.piece_length);
        let mut end: u64 = begin + u64::from(self.piece_length);

        // Prevent unbounded values
        if end > self.length {
            end = self.length;
        }

        Ok((end - begin) as u32)
    }

    /// Get piece offset.
    ///
    /// # Arguments
    ///
    /// * `index` - The piece index.
    ///
    fn get_piece_offset(&self, index: u32) -> u32 {
        index * self.piece_length
    }
}
