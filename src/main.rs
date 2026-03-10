//! # Xerus BitTorrent Client
//!
//! A command-line BitTorrent client written in Rust.
//!
//! ## Features
//!
//! - Core BitTorrent protocol implementation
//! - Magnet link support (BEP 9, BEP 10)
//! - Multitracker support
//! - Multi-peer concurrent downloading
//! - Piece verification with SHA-1 hashing
//! - Progress tracking with visual progress bar
//! - Robust error handling and reconnection logic
//!
//! ## Usage
//!
//! ```bash
//! xerus <torrent_file> -o <output_file>
//! xerus "<magnet_link>" -o <output_file>
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
mod magnet;
mod message;
mod peer;
mod piece;
mod torrent;
mod tracker;
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
    /// Path to torrent file or magnet link
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
    let input = args.torrent.trim().to_lowercase();

    let mut torrent = Torrent::new();

    if input.starts_with("magnet:") {
        torrent.open_magnet(&args.torrent)?;
    } else {
        let file_path = args.torrent.trim();
        if !Path::new(file_path).exists() {
            return Err(anyhow!("could not find torrent file: {}", file_path));
        }
        torrent.open_torrent(PathBuf::from(file_path))?;
    }

    // Download torrent data
    let data = torrent.download()?;

    // Handle multi-file vs single-file torrents
    if torrent.is_multi_file() {
        write_multi_file(&torrent, &data, args.output.as_deref())?;
    } else {
        write_single_file(&torrent, &data, args.output.as_deref())?;
    }

    Ok(())
}

/// Writes downloaded data to a single file.
fn write_single_file(torrent: &Torrent, data: &[u8], output: Option<&str>) -> Result<()> {
    let default_filename = sanitize_filename(torrent.name());
    let output_filename = output.unwrap_or(&default_filename);
    let output_filepath = PathBuf::from(output_filename);

    // Check if output file already exists
    if output_filepath.exists() && !confirm_overwrite(output_filename)? {
        println!("Download cancelled.");
        return Ok(());
    }

    let mut output_file = File::create(&output_filepath)
        .map_err(|e| anyhow!("could not create output file '{}': {}", output_filename, e))?;

    output_file
        .write_all(data)
        .map_err(|e| anyhow!("could not write data to file '{}': {}", output_filename, e))?;

    println!("Saved in \"{}\".", output_filename);
    Ok(())
}

/// Writes downloaded data to multiple files in a directory structure.
fn write_multi_file(torrent: &Torrent, data: &[u8], output: Option<&str>) -> Result<()> {
    let base_dir = output
        .map(|s| s.to_string())
        .unwrap_or_else(|| sanitize_filename(torrent.name()));
    let base_path = PathBuf::from(&base_dir);

    // Check if base directory already exists
    if base_path.exists() && !confirm_overwrite(&base_dir)? {
        println!("Download cancelled.");
        return Ok(());
    }

    // Create base directory
    std::fs::create_dir_all(&base_path)
        .map_err(|e| anyhow!("could not create directory '{}': {}", base_dir, e))?;

    // Write each file at its correct offset
    let mut offset: u64 = 0;
    for file_info in torrent.files() {
        // Build file path from path components
        let mut file_path = base_path.clone();
        for component in &file_info.path {
            file_path.push(sanitize_filename(component));
        }

        // Create parent directories if needed
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("could not create directory '{}': {}", parent.display(), e))?;
        }

        // Calculate byte range for this file
        let start = offset as usize;
        let end = (offset + file_info.length) as usize;

        // Bounds check
        if end > data.len() {
            return Err(anyhow!(
                "file '{}' extends beyond downloaded data (offset {} + length {} > {})",
                file_path.display(),
                offset,
                file_info.length,
                data.len()
            ));
        }

        // Write file data
        let mut output_file = File::create(&file_path)
            .map_err(|e| anyhow!("could not create file '{}': {}", file_path.display(), e))?;

        output_file
            .write_all(&data[start..end])
            .map_err(|e| anyhow!("could not write to file '{}': {}", file_path.display(), e))?;

        offset += file_info.length;
    }

    println!("Saved in \"{}\".", base_dir);
    Ok(())
}

/// Prompts user to confirm overwriting existing file/directory.
fn confirm_overwrite(name: &str) -> Result<bool> {
    println!("'{}' already exists.", name);
    print!("Do you want to overwrite it? (y/N): ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
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
