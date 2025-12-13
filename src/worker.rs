//! # BitTorrent Worker Thread
//!
//! This module implements the worker thread that manages downloading from a single peer.
//! Each worker handles one peer connection and coordinates piece downloads through channels.
//!
//! ## Worker Architecture
//!
//! - **One worker per peer**: Each peer gets its own thread for concurrent downloads
//! - **Channel-based coordination**: Uses crossbeam channels for work distribution
//! - **Piece pipeline**: Receives work, downloads pieces, verifies integrity, sends results
//!
//! ## Download Process
//!
//! 1. **Connection**: Establish TCP connection and perform BitTorrent handshake
//! 2. **Bitfield Exchange**: Learn which pieces the peer has
//! 3. **Choke Management**: Handle choke/unchoke states for flow control
//! 4. **Piece Requests**: Request blocks in parallel (up to 5 concurrent requests)
//! 5. **Data Assembly**: Collect blocks into complete pieces
//! 6. **Verification**: SHA-1 hash verification against torrent metadata
//! 7. **Result Reporting**: Send completed pieces back via channel
//!
//! ## Error Handling
//!
//! - Connection timeouts and automatic reconnection
//! - Retry logic for handshake and bitfield reading
//! - Graceful handling of choked peers
//! - Piece verification failures trigger redownload
//!
//! ## Performance Optimizations
//!
//! - Pipelined requests: Send multiple block requests before receiving responses
//! - Block size: 16KB blocks for efficient network usage
//! - Concurrent downloads: Multiple workers download different pieces simultaneously

use crate::client::*;
use crate::message::*;
use crate::peer::*;
use crate::piece::*;

use anyhow::{anyhow, Result};
use boring::sha::Sha1;
use crossbeam_channel::{Receiver, Sender};
use std::thread;
use std::time::Duration;

// Maximum number of concurrent block requests per peer
const NB_REQUESTS_MAX: u32 = 5;

// Standard block size for piece downloads (16KB)
const BLOCK_SIZE_MAX: u32 = 16384;

/// Manages downloading from a single BitTorrent peer.
///
/// Each worker runs in its own thread and handles the complete download lifecycle
/// for one peer, from connection establishment to piece verification.
pub struct Worker {
    /// Information about the remote peer (IP, port, ID)
    peer: Peer,
    /// 20-byte unique identifier for this client instance
    peer_id: Vec<u8>,
    /// 20-byte SHA-1 hash of the torrent's info dictionary
    info_hash: Vec<u8>,
    /// Channel for receiving piece work assignments and returning unassigned work
    work_chan: (Sender<PieceWork>, Receiver<PieceWork>),
    /// Channel for sending completed piece results
    result_chan: (Sender<PieceResult>, Receiver<PieceResult>),
}

impl Worker {
    /// Creates a new worker instance for managing downloads from a peer.
    ///
    /// Sets up the worker with peer information and communication channels.
    /// The worker will establish its own connection to the peer when started.
    ///
    /// # Arguments
    ///
    /// * `peer` - Peer information including IP address and port
    /// * `peer_id` - 20-byte unique identifier for this client
    /// * `info_hash` - 20-byte SHA-1 hash of the torrent info dictionary
    /// * `work_chan` - Tuple of (sender, receiver) for piece work coordination
    /// * `result_chan` - Tuple of (sender, receiver) for completed piece results
    ///
    /// # Returns
    ///
    /// Returns a `Result<Worker>` with the configured worker instance.
    pub fn new(
        peer: Peer,
        peer_id: Vec<u8>,
        info_hash: Vec<u8>,
        work_chan: (Sender<PieceWork>, Receiver<PieceWork>),
        result_chan: (Sender<PieceResult>, Receiver<PieceResult>),
    ) -> Result<Worker> {
        // Create a new worker
        let worker = Worker {
            peer,
            peer_id,
            info_hash,
            work_chan,
            result_chan,
        };

        Ok(worker)
    }

    /// Start worker.
    pub fn start_download(&self) {
        let peer_copy = self.peer.clone();
        let peer_id_copy = self.peer_id.clone();
        let info_hash_copy = self.info_hash.clone();

        // Store peer id for logging
        let pid = peer_copy.id;

        // Create new client
        let mut client = match Client::new(peer_copy, peer_id_copy, info_hash_copy) {
            Ok(client) => client,
            Err(_) => return,
        };

        // Set connection timeout
        if client.set_connection_timeout(5).is_err() {
            return;
        }

        // Handshake with peer with retry logic
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_SECS: u64 = 5;

        debug!("Attempting to connect to peer {:?}", pid);
        while retry_count < MAX_RETRIES {
            match client.handshake_with_peer() {
                Ok(_) => {
                    debug!("Successfully connected to peer {:?}", pid);
                    break; // Success, exit retry loop
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count < MAX_RETRIES {
                        debug!(
                            "Handshake failed (attempt {}/{}), retrying in {} seconds: {}",
                            retry_count, MAX_RETRIES, RETRY_DELAY_SECS, e
                        );
                        thread::sleep(Duration::from_secs(RETRY_DELAY_SECS));

                        // Try to reconnect before retrying
                        if let Err(connect_err) = client.reconnect() {
                            debug!("Reconnection failed: {}", connect_err);
                            continue;
                        }
                    } else {
                        debug!(
                            "Max handshake retries ({}) exceeded for peer {:?}, giving up: {}",
                            MAX_RETRIES, pid, e
                        );
                        return;
                    }
                }
            }
        }

        // Read bitfield from peer with retry logic
        let mut retry_count = 0;
        while retry_count < MAX_RETRIES {
            match client.read_bitfield() {
                Ok(_) => break, // Success, exit retry loop
                Err(e) => {
                    retry_count += 1;
                    if retry_count < MAX_RETRIES {
                        debug!(
                            "Reading bitfield failed (attempt {}/{}), retrying in {} seconds: {}",
                            retry_count, MAX_RETRIES, RETRY_DELAY_SECS, e
                        );
                        thread::sleep(Duration::from_secs(RETRY_DELAY_SECS));

                        // Try to reconnect before retrying
                        if let Err(connect_err) = client.reconnect() {
                            debug!("Reconnection failed: {}", connect_err);
                            continue;
                        }
                    } else {
                        debug!(
                            "Max bitfield read retries ({}) exceeded, giving up: {}",
                            MAX_RETRIES, e
                        );
                        return;
                    }
                }
            }
        }

        // Send unchoke
        if client.send_unchoke().is_err() {
            return;
        }

        // Send interested
        if client.send_interested().is_err() {
            return;
        }

        loop {
            // Receive a piece from work channel
            let mut piece_work: PieceWork = match self.work_chan.1.recv() {
                Ok(piece_work) => piece_work,
                Err(_) => {
                    // Channel is closed or empty, exit worker gracefully
                    info!("Worker exiting: work channel closed or empty");
                    return;
                }
            };

            // Check if remote peer has piece
            if !client.has_piece(piece_work.index) {
                // Resend piece to work channel
                if self.work_chan.0.send(piece_work).is_err() {
                    error!("Error: could not send piece to channel");
                    return;
                }
                continue;
            }

            // Download piece
            if self.download_piece(&mut client, &mut piece_work).is_err() {
                // Resend piece to work channel
                if self.work_chan.0.send(piece_work).is_err() {
                    error!("Error: could not send piece to channel");
                    return;
                }
                return;
            }

            // Verify piece integrity
            if self.verify_piece_integrity(&mut piece_work).is_err() {
                // Resend piece to work channel
                if self.work_chan.0.send(piece_work).is_err() {
                    error!("Error: could not send piece to channel");
                    return;
                }
                continue;
            }

            // Notify peer that piece was downloaded
            if client.send_have(piece_work.index).is_err() {
                error!("Error: could not notify peer that piece was downloaded");
            }

            // Send piece to result channel
            let piece_result =
                PieceResult::new(piece_work.index, piece_work.length, piece_work.data);
            if self.result_chan.0.send(piece_result).is_err() {
                error!("Error: could not send piece to channel");
                return;
            }
        }
    }

    /// Download a torrent piece.
    ///
    /// # Arguments
    ///
    /// * `client` - A client connected to a remote peer.
    /// * `piece_work` - A piece to download.
    ///
    fn download_piece(&self, client: &mut Client, piece_work: &mut PieceWork) -> Result<()> {
        // Set client connection timeout
        client.set_connection_timeout(120)?;

        // Reset piece counters
        piece_work.requests = 0;
        piece_work.requested = 0;
        piece_work.downloaded = 0;

        // Download torrent piece
        while piece_work.downloaded < piece_work.length {
            // If client is unchoked by peer
            if !client.is_choked() {
                while piece_work.requests < NB_REQUESTS_MAX
                    && piece_work.requested < piece_work.length
                {
                    // Get block size to request
                    let mut block_size = BLOCK_SIZE_MAX;
                    let remaining = piece_work.length - piece_work.requested;
                    if remaining < BLOCK_SIZE_MAX {
                        block_size = remaining;
                    }

                    // Send request for a block
                    client.send_request(piece_work.index, piece_work.requested, block_size)?;

                    // Update number of requests sent
                    piece_work.requests += 1;

                    // Update size of requested data
                    piece_work.requested += block_size;
                }
            }

            // Listen peer
            let message: Message = client.read_message()?;

            // Parse message
            match message.id {
                MESSAGE_CHOKE => {
                    client.read_choke();
                    warn!("Peer choked us, waiting for unchoke...");
                }
                MESSAGE_UNCHOKE => {
                    client.read_unchoke();
                    info!("Peer unchoked us, resuming downloads");
                }
                MESSAGE_HAVE => client.read_have(message)?,
                MESSAGE_PIECE => client.read_piece(message, piece_work)?,
                MESSAGE_KEEPALIVE => {
                    // Handle keep-alive message
                    info!("Received keep-alive from peer");
                }
                _ => info!("received unknown message from peer"),
            }
        }

        info!("Successfully downloaded piece {:?}", piece_work.index);

        Ok(())
    }

    /// Verify the integrity of a downloaded torrent piece.
    ///
    /// # Arguments
    ///
    /// * `piece_work` - A piece to download.
    ///
    fn verify_piece_integrity(&self, piece_work: &mut PieceWork) -> Result<()> {
        // Hash piece data
        let mut hasher = Sha1::new();
        hasher.update(&piece_work.data);

        // Read hash digest
        let hash = hasher.finish().to_vec();

        // Compare hashes
        if hash != piece_work.hash {
            return Err(anyhow!(
                "could not verify integrity of piece downloaded from peer"
            ));
        }

        info!(
            "Successfully verified integrity of piece {:?}",
            piece_work.index
        );

        Ok(())
    }
}
