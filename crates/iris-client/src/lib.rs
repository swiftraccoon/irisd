//! The rules a viewer of irisd applies to the origin it reaches the daemon at
//! and to what the daemon serves, free of any toolkit. The Linux viewer builds
//! on the crate, which is shaped for the planned iOS and macOS viewer that
//! `docs/research/15-apple-clients.md` designs. The crate does no input or
//! output: a viewer keeps the bearer and carries its requests and live feeds
//! with its transport.
//!
//! The modules:
//!
//! - [`origin`] accepts the API origin under a plain-HTTP policy and gives a
//!   stream's live URL on it.
//! - [`api`] builds the API's requests on that origin and checks their
//!   answers. It holds the answers as a viewer decodes them, the bounds it
//!   checks them against, and the rules they pass.
//! - [`vod`] checks a VOD playlist, reads its `#IRIS-FRAGMENT` timing, and
//!   rewrites its references for a player.
//! - [`timeline_model`] maps a wall-clock time to a position in that playlist
//!   and back, finds the event nearest a time, and places times on a bar.
//! - [`playback`] holds a recorded window's playlists as one selection, names
//!   the bodies its player is handed, and gives the times of its thumbnails
//!   and the name of its export.
//! - [`live`] reads the framing of a live feed's messages and the media time
//!   a live fragment starts with. It reads each message into a decoder
//!   configuration or one sample placed in the message, decides when the feed
//!   connects again, and bounds the payload bytes a viewer holds.
//! - [`sound_clock`] places a camera's picture and sound on one clock by
//!   their media times.
//!
//! The checks and readers take what a caller was given or fetched and return
//! a value or a refusal, and none panics on what the network sends. The Linux
//! viewer (`iris-gui`) carries its requests and its live feeds with the
//! transport of `iris-transport`. It draws its timeline over
//! [`timeline_model::TimelineModel`], serves the bodies of a
//! [`playback::PlaybackSnapshot`] to its player, and sets the offsets of its
//! live feeds from [`sound_clock::SoundClock`]. The design of the planned
//! viewer carries its requests and live feeds through its platform.
//!
//! # Example
//!
//! ```
//! use iris_client::{live, vod};
//!
//! let body = "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=1000000,DURATION-TICKS=90000\n";
//! let fragments = vod::playlist_fragments(body)?;
//! // One second in 90 kHz ticks.
//! assert_eq!(fragments[0].duration.get(), 90_000);
//! // An init segment has no media time.
//! assert_eq!(live::media_time(b"\0\0\0\x1cftypiso6"), None);
//! # Ok::<(), vod::PlaylistError>(())
//! ```

pub mod api;
pub mod live;
pub mod origin;
pub mod playback;
pub mod sound_clock;
pub mod timeline_model;
pub mod vod;

/// Compiles and runs the example of `README.md` as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
