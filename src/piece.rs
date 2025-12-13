//! # BitTorrent Piece Management
//!
//! This module defines structures for managing piece downloads in the BitTorrent protocol.
//! Pieces are verification units of the torrent data, typically 256KB-1MB in size.
//! They are downloaded in smaller blocks (16KB) for efficiency.
//!
//! ## Piece Download Process
//!
//! 1. **PieceWork**: Tracks the download state of a single piece
//! 2. **PieceResult**: Represents a completed piece ready for verification
//! 3. **Block Requests**: Pieces are downloaded in smaller blocks (16KB) for efficiency
//!
//! ## Piece Verification
//!
//! Each piece has a SHA-1 hash stored in the torrent metadata. After download,
//! the piece data is hashed and compared to ensure integrity.
//!
//! ## Download State Tracking
//!
//! PieceWork maintains counters for:
//! - Number of block requests sent
//! - Total bytes requested vs downloaded
//! - Piece data buffer for assembly

/// Tracks the download progress and state of a single piece.
///
/// Manages the lifecycle of downloading one piece from multiple peers,
/// including block requests, data assembly, and progress tracking.
#[derive(Default, Debug, Clone)]
pub struct PieceWork {
    /// Zero-based index of this piece in the torrent
    pub index: u32,
    /// SHA-1 hash of the piece for verification (20 bytes)
    pub hash: Vec<u8>,
    /// Total length of the piece in bytes
    pub length: u32,
    /// Buffer containing downloaded piece data (initialized to zeros)
    pub data: Vec<u8>,
    /// Number of block requests sent for this piece
    pub requests: u32,
    /// Total bytes requested from peers
    pub requested: u32,
    /// Total bytes successfully downloaded and stored
    pub downloaded: u32,
}

/// Represents a completed piece that has been fully downloaded.
///
/// Contains the piece data and metadata, ready for hash verification
/// and writing to the output file.
#[derive(Default, Debug, Clone)]
pub struct PieceResult {
    /// Zero-based index of this piece in the torrent
    pub index: u32,
    /// Total length of the piece in bytes
    pub length: u32,
    /// Complete piece data buffer
    pub data: Vec<u8>,
}

impl PieceWork {
    /// Creates a new PieceWork instance for downloading a piece.
    ///
    /// Initializes the piece data buffer with the correct size and sets up
    /// tracking counters for download progress.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index in the torrent
    /// * `hash` - SHA-1 hash bytes for piece verification
    /// * `length` - Total size of the piece in bytes
    ///
    /// # Returns
    ///
    /// A new `PieceWork` instance ready for download coordination.
    pub fn new(index: u32, hash: Vec<u8>, length: u32) -> PieceWork {
        PieceWork {
            index,
            hash,
            length,
            data: vec![0; length as usize],
            requests: 0,
            requested: 0,
            downloaded: 0,
        }
    }
}

impl PieceResult {
    /// Creates a new PieceResult from completed download data.
    ///
    /// Used when a piece has been fully assembled and is ready for
    /// verification and storage.
    ///
    /// # Arguments
    ///
    /// * `index` - Zero-based piece index in the torrent
    /// * `length` - Total size of the piece in bytes
    /// * `data` - Complete piece data buffer
    ///
    /// # Returns
    ///
    /// A new `PieceResult` instance containing the piece data.
    pub fn new(index: u32, length: u32, data: Vec<u8>) -> PieceResult {
        PieceResult {
            index,
            length,
            data,
        }
    }
}
