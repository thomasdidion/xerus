# xerus

[![Build Status](https://github.com/zenoxygen/xerus/actions/workflows/ci.yaml/badge.svg)](https://github.com/zenoxygen/xerus/actions/workflows/ci.yaml)
[![Crates.io](https://img.shields.io/crates/v/xerus.svg)](https://crates.io/crates/xerus)
[![Docs](https://img.shields.io/docsrs/xerus/latest)](https://docs.rs/xerus/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A command-line BitTorrent client, written in Rust.

## Usage

```
xerus 0.2.0
zenoxygen <zenoxygen@protonmail.com>
A command-line BitTorrent client, written in Rust.

USAGE:
    xerus [OPTIONS] <torrent>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -o, --output <output>    Output filename (defaults to name from torrent)

ARGS:
    <torrent>    Path to the .torrent file
```

## Example

Try to download an official Debian ISO image:

```
$> xerus debian-13.2.0-amd64-netinst.iso.torrent
Downloading "debian-13.2.0-amd64-netinst.iso" (3136 pieces)
Saved in "debian-13.2.0-amd64-netinst.iso".
```

And verify the checksum matches that expected from the checksum file:

```
$> sha512sum -c SHA512SUM | grep debian-13.2.0-amd64-netinst.iso
debian-13.2.0-amd64-netinst.iso: OK
```

## Installation

### From crates.io (recommended)

```bash
cargo install xerus
```

### From source

```bash
cargo install --path .
```

## Uninstallation

```bash
cargo uninstall xerus
```

## Debug

Run with the environment variable set:

```
$> RUST_LOG=trace xerus <torrent>
```

## Documentation

Learn more here: [https://docs.rs/xerus](https://docs.rs/xerus).

## License

Xerus is distributed under the terms of the [MIT License](LICENSE).
