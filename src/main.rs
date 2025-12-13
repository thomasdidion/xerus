//! # Xerus BitTorrent Client
//!
//! A command-line BitTorrent client written in Rust.
//!
//! ## Features
//!
//! - Core BitTorrent protocol implementation
//! - Basic multitracker support
//! - Multi-peer concurrent downloading
//! - Piece verification with SHA-1 hashing
//! - Progress tracking with visual progress bar
//! - Robust error handling and reconnection logic
//!
//! ## Usage
//!
//! ```bash
//! xerus <torrent_file>
//! xerus <torrent_file> -o <output_file>
//! ```
//!
//! ## Architecture
//!
//! The client follows a multi-threaded architecture:
//!
//! - **Main thread**: Parses arguments, loads torrent, coordinates download
//! - **Worker threads**: Each handles communication with one peer
//! - **Channels**: Coordinate piece work distribution and result collection

#[macro_use]
extern crate log;

mod client;
mod handshake;
mod message;
mod peer;
mod piece;
mod torrent;
mod worker;

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::Parser;
use torrent::*;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "A command-line BitTorrent client, written in Rust."
)]
struct Args {
    /// Path to the .torrent file
    torrent: String,

    /// Output filename (defaults to name from torrent)
    #[arg(short = 'o', long)]
    output: Option<String>,
}

/// Sanitize a filename to prevent path traversal and basic issues.
fn sanitize_filename(filename: &str) -> String {
    // Replace path separators with underscores to prevent directory traversal
    let safe_name = filename.replace(['/', '\\'], "_");

    // Use default name if empty
    if safe_name.trim().is_empty() {
        "download".to_string()
    } else {
        safe_name
    }
}

fn run(args: Args) -> Result<()> {
    // Get torrent file path
    let torrent = &args.torrent;

    // Get output file path (optional, defaults to torrent name)
    let output_file = args.output.as_deref();

    // Check if torrent file exists
    if !Path::new(&torrent).exists() {
        return Err(anyhow!("could not find torrent file: {}", torrent));
    }

    let torrent_filepath = PathBuf::from(torrent);

    // Open torrent to get metadata (needed for default filename)
    let mut temp_torrent = Torrent::new();
    temp_torrent.open(torrent_filepath.clone())?;
    let raw_filename = temp_torrent.name().to_string();

    // Sanitize the filename to make it safe
    let default_filename = sanitize_filename(&raw_filename);

    // Determine output filename
    let output_filename = output_file.unwrap_or(&default_filename);
    let output_filepath = PathBuf::from(output_filename);

    // Check if output file already exists
    if output_filepath.exists() {
        println!("Output file '{}' already exists.", output_filename);
        print!("Do you want to overwrite it? (y/N): ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        if input != "y" && input != "yes" {
            println!("Download cancelled.");
            return Ok(());
        }
    }

    // Create output file
    let mut output_file = match File::create(&output_filepath) {
        Ok(file) => file,
        Err(e) => {
            return Err(anyhow!(
                "could not create output file '{}': {}",
                output_filename,
                e
            ))
        }
    };

    // Download torrent
    let mut torrent = Torrent::new();
    torrent.open(torrent_filepath)?;
    let data: Vec<u8> = torrent.download()?;

    // Save data to file
    if let Err(e) = output_file.write(&data) {
        return Err(anyhow!(
            "could not write data to file '{}': {}",
            output_filename,
            e
        ));
    }

    println!("Saved in \"{}\".", output_filename);

    Ok(())
}

fn main() {
    // Initialize logger
    pretty_env_logger::init_timed();

    // Parse arguments
    let args = Args::parse();

    // Run program, eventually exit failure
    if let Err(error) = run(args) {
        eprintln!("Error: {}", error);
        std::process::exit(1);
    }

    // Exit success
    std::process::exit(0);
}
