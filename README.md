# SJNmusic

SJNmusic is a self-hosted music library and playback system with a local daemon, command-line interface, and graphical user interface.

It is designed to provide a fast and lightweight way to manage a personal music collection, create playlists, and control playback from multiple clients.

## Features

* Local music library
* Fast fuzzy search
* Playlist management
* Playback queue
* HTTP daemon (`sjnmusicd`)
* Command-line client (`sjnmusic`)
* Graphical user interface
* Asynchronous downloads
* Local metadata database
* Shuffle and repeat modes
* Playback history and statistics
* JSON output for scripting and automation

## Components

| Component   | Description                                                      |
| ----------- | ---------------------------------------------------------------- |
| `sjnmusicd` | Background daemon responsible for the music library and playback |
| `sjnmusic`  | Command-line client communicating with the daemon                |
| GUI         | Graphical interface for everyday use                             |

## Installation

Currently there are no official packages.

Clone the repository and build using Cargo:

```bash
git clone <repository-url>
cd SJNmusic
cargo build --release
```

## Basic Usage

Start the daemon:

```bash
target/release/sjnmusicd
```

Show all songs:

```bash
cli/sjnmusic songs
```

Play a song:

```bash
cli/sjnmusic play "Haftbefehl - RADW"
```

Create a playlist:

```bash
cli/sjnmusic pl-new "Deutschrap"
```

Show playback status:

```bash
cli/sjnmusic status
```

## Download Support

SJNmusic can optionally use external tools such as `yt-dlp` to import media into the local music library.

Downloaded media becomes part of the user's local library and can then be searched, organized, and played like any other local file.

SJNmusic itself does **not** provide, host, distribute, or include any copyrighted media.

## Legal Notice

Users are solely responsible for ensuring that any media they access or download complies with applicable copyright laws and the terms of service of the platforms they use.

SJNmusic is a general-purpose music management application. It is intended for managing media that the user is legally permitted to access and store.

## Project Status

SJNmusic is currently under active development.

Interfaces, commands, APIs, and file formats may change between releases.

Bug reports, feature requests, and pull requests are welcome.

## License

This project is licensed under the MIT License.
