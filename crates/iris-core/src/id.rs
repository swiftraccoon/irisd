//! Identifiers for cameras and streams.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
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
    ///
    /// # Errors
    ///
    /// [`InvalidCameraId`] when `id` is empty, longer than 32 bytes, or holds a
    /// character outside `[a-z0-9_-]`.
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
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
    /// The audio stream. Reserved: catalog types carry it, and configuration
    /// rejects recording it.
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

/// Durable random identity of one recording root: 16 bytes, written to the
/// root's marker file and to the catalog as 32 lowercase hexadecimal
/// characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootIdentity([u8; 16]);

/// A string that is not a recording-root identity: anything but exactly 32
/// lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("recording-root identity must be exactly 32 lowercase hexadecimal characters")]
pub struct InvalidRootIdentity;

impl RootIdentity {
    /// Characters in the text form.
    pub const HEX_LEN: usize = 32;

    /// Wraps the identity's bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The identity's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for RootIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for RootIdentity {
    type Err = InvalidRootIdentity;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != Self::HEX_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(InvalidRootIdentity);
        }
        let mut identity = [0u8; 16];
        for (index, output) in identity.iter_mut().enumerate() {
            *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| InvalidRootIdentity)?;
        }
        Ok(Self(identity))
    }
}

/// A camera id paired with a stream kind: the unit that owns a recording.
/// Serializes as `{"camera": …, "kind": …}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    fn ids_and_keys_display_as_they_are_written_in_paths_and_logs() {
        let camera = CameraId::try_new("cam1").unwrap();
        assert_eq!(camera.as_str(), "cam1");
        assert_eq!(camera.to_string(), "cam1");
        assert_eq!(StreamKind::Main.to_string(), "main");
        assert_eq!(StreamKind::Sub.to_string(), "sub");
        let key = StreamKey {
            camera,
            kind: StreamKind::Sub,
        };
        assert_eq!(key.to_string(), "cam1/sub");
    }

    #[test]
    fn stream_keys_serialize_as_camera_and_kind() {
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Sub,
        };
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, r#"{"camera":"cam1","kind":"sub"}"#);
        assert_eq!(serde_json::from_str::<StreamKey>(&json).unwrap(), key);
        assert!(serde_json::from_str::<StreamKey>(r#"{"camera":"Cam/1","kind":"sub"}"#).is_err());
    }

    #[test]
    fn root_identities_round_trip_through_lowercase_hex() {
        let identity = RootIdentity::from_bytes([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ]);
        let text = identity.to_string();
        assert_eq!(text, "0123456789abcdeffedcba9876543210");
        assert_eq!(text.parse::<RootIdentity>().unwrap(), identity);
        for bad in [
            "",
            "0123456789abcdeffedcba987654321",
            "0123456789abcdeffedcba98765432100",
            "0123456789ABCDEFFEDCBA9876543210",
            "0123456789abcdeffedcba987654321g",
        ] {
            assert_eq!(
                bad.parse::<RootIdentity>(),
                Err(InvalidRootIdentity),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn stream_kind_round_trips_through_str() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            assert_eq!(kind.as_str().parse::<StreamKind>().unwrap(), kind);
        }
        assert!("video".parse::<StreamKind>().is_err());
    }
}
