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
    let torrent_path = &args.torrent;

    if !Path::new(torrent_path).exists() {
        return Err(anyhow!("could not find torrent file: {}", torrent_path));
    }

    // Open torrent file
    let mut torrent = Torrent::new();
    torrent.open(PathBuf::from(torrent_path))?;

    // Determine output filename
    let default_filename = sanitize_filename(torrent.name());
    let output_filename = args.output.as_deref().unwrap_or(&default_filename);
    let output_filepath = PathBuf::from(output_filename);

    // Check if output file already exists
    if output_filepath.exists() {
        println!("Output file '{}' already exists.", output_filename);
        print!("Do you want to overwrite it? (y/N): ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Download cancelled.");
            return Ok(());
        }
    }

    // Download torrent
    let data = torrent.download()?;

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

    if let Err(e) = output_file.write_all(&data) {
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

    if let Err(error) = run(args) {
        eprintln!("Error: {}", error);
        std::process::exit(1);
    }
}
