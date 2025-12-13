//! # Command-Line Argument Parsing
//!
//! This module handles parsing and validation of command-line arguments.
//!
//! ## Arguments
//!
//! - `<torrent>`: Path to the .torrent file (required)
//! - `-o, --output <output>`: Output filename (defaults to name from torrent)
//!
//! ## Example
//!
//! ```bash
//! xerus debian.iso.torrent
//! xerus debian.iso.torrent -o debian.iso
//! ```

use clap::{crate_name, crate_version, App, Arg};

/// Parse command-line arguments.
pub fn parse_args<'a>() -> clap::ArgMatches<'a> {
    App::new(crate_name!())
        .version(crate_version!())
        .about("A command-line BitTorrent client, written in Rust.")
        .author("zenoxygen <zenoxygen@protonmail.com>")
        .arg(
            Arg::with_name("torrent")
                .help("Path to the .torrent file")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("output")
                .short("o")
                .long("output")
                .help("Output filename (defaults to name from torrent)")
                .takes_value(true),
        )
        .get_matches()
}
