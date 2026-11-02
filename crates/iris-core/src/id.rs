//! Identifiers for cameras and streams.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A camera identifier: 1 to 32 characters from `[a-z0-9_-]`, usable as a
/// directory name and a URL path component without escaping.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CameraId(String);

/// Error returned when a camera id is malformed.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("camera id {0:?} must be 1-32 characters from [a-z0-9_-]")]
pub struct InvalidCameraId(String);

impl CameraId {
    /// Validates and wraps an id.
    pub fn try_new(id: &str) -> Result<Self, InvalidCameraId> {
        let valid_len = (1..=32).contains(&id.len());
        let valid_chars = id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if valid_len && valid_chars {
            Ok(Self(id.to_owned()))
        } else {
            Err(InvalidCameraId(id.to_owned()))
        }
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CameraId {
    type Err = InvalidCameraId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_new(s)
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CameraId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_new(&raw).map_err(serde::de::Error::custom)
    }
}

/// Which of a camera's elementary streams a recording holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// The full-resolution video stream.
    Main,
    /// The low-resolution video stream used for grids and thumbnails.
    Sub,
    /// The audio stream, stored in its own file.
    Audio,
}

impl StreamKind {
    /// Returns the directory name used for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Sub => "sub",
            Self::Audio => "audio",
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a stream kind name is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("unknown stream kind {0:?}; expected main, sub, or audio")]
pub struct UnknownStreamKind(String);

impl FromStr for StreamKind {
    type Err = UnknownStreamKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "main" => Ok(Self::Main),
            "sub" => Ok(Self::Sub),
            "audio" => Ok(Self::Audio),
            other => Err(UnknownStreamKind(other.to_owned())),
        }
    }
}

/// A camera id paired with a stream kind: the unit that owns a recording.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamKey {
    /// The owning camera.
    pub camera: CameraId,
    /// Which elementary stream.
    pub kind: StreamKind,
}

impl fmt::Display for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.camera, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_ids_and_rejects_invalid_ones() {
        assert!(CameraId::try_new("cam1").is_ok());
        assert!(CameraId::try_new("front-door_2").is_ok());
        assert!(CameraId::try_new("").is_err());
        assert!(CameraId::try_new("Cam1").is_err());
        assert!(CameraId::try_new("a/b").is_err());
        assert!(CameraId::try_new(&"x".repeat(33)).is_err());
    }

    #[test]
    fn stream_kind_round_trips_through_str() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            assert_eq!(kind.as_str().parse::<StreamKind>().unwrap(), kind);
        }
        assert!("video".parse::<StreamKind>().is_err());
    }
}
