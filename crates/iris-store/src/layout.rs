//! On-disk naming of recording files.

use std::path::{Path, PathBuf};

use iris_core::id::StreamKey;
use iris_core::time::WallUs;

/// Returns `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4` for a file
/// whose first fragment is at `start` (UTC).
#[must_use]
pub fn segment_path(root: &Path, key: &StreamKey, start: WallUs) -> PathBuf {
    let c = start.to_civil();
    root.join(key.camera.as_str())
        .join(key.kind.as_str())
        .join(format!("{:04}-{:02}-{:02}", c.year, c.month, c.day))
        .join(format!(
            "{:02}{:02}{:02}.{:06}Z.mp4",
            c.hour, c.minute, c.second, c.micros
        ))
}

/// Returns the sidecar index path for a recording file.
#[must_use]
pub fn index_path(mp4: &Path) -> PathBuf {
    mp4.with_extension("idx")
}

/// Largest `n` of a numbered sibling: a name taken by that many recordings
/// of the same microsecond is refused.
pub const MAX_NUMBERED_SIBLING: u32 = 99;

/// Returns `<stem>-<n>.mp4` beside `path`: the name a recording takes when
/// `path` is already claimed, for `n` from 1 to [`MAX_NUMBERED_SIBLING`].
#[must_use]
pub fn numbered_segment_path(path: &Path, n: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("segment");
    path.with_file_name(format!("{stem}-{n}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::id::{CameraId, StreamKind};

    #[test]
    fn builds_dated_paths() {
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        // 2026-10-31T15:35:58Z
        let path = segment_path(Path::new("/r"), &key, WallUs::new(1_793_460_958_000_000));
        assert_eq!(
            path,
            PathBuf::from("/r/cam1/main/2026-10-31/153558.000000Z.mp4")
        );
        assert_eq!(
            index_path(&path),
            PathBuf::from("/r/cam1/main/2026-10-31/153558.000000Z.idx")
        );
    }

    #[test]
    fn numbers_a_sibling_beside_the_taken_name() {
        let path = PathBuf::from("/r/cam1/main/2026-10-31/153558.000000Z.mp4");
        assert_eq!(
            numbered_segment_path(&path, 1),
            PathBuf::from("/r/cam1/main/2026-10-31/153558.000000Z-1.mp4")
        );
        assert_eq!(
            index_path(&numbered_segment_path(&path, 99)),
            PathBuf::from("/r/cam1/main/2026-10-31/153558.000000Z-99.idx")
        );
    }

    #[test]
    fn includes_microseconds_in_the_canonical_name() {
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        let first = segment_path(Path::new("/r"), &key, WallUs::new(1_793_460_958_000_001));
        let second = segment_path(Path::new("/r"), &key, WallUs::new(1_793_460_958_000_002));
        assert_ne!(first, second);
        assert!(first.ends_with("153558.000001Z.mp4"));
        assert!(second.ends_with("153558.000002Z.mp4"));
    }
}
