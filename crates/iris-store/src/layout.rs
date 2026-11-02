//! On-disk naming of recording files.

use std::path::{Path, PathBuf};

use iris_core::id::StreamKey;
use iris_core::time::WallUs;

/// Returns `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS>Z.mp4` for a file whose
/// first fragment is at `start` (UTC).
#[must_use]
pub fn segment_path(root: &Path, key: &StreamKey, start: WallUs) -> PathBuf {
    let c = start.to_civil();
    root.join(key.camera.as_str())
        .join(key.kind.as_str())
        .join(format!("{:04}-{:02}-{:02}", c.year, c.month, c.day))
        .join(format!("{:02}{:02}{:02}Z.mp4", c.hour, c.minute, c.second))
}

/// Returns the sidecar index path for a recording file.
#[must_use]
pub fn index_path(mp4: &Path) -> PathBuf {
    mp4.with_extension("idx")
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
        assert_eq!(path, PathBuf::from("/r/cam1/main/2026-10-31/153558Z.mp4"));
        assert_eq!(
            index_path(&path),
            PathBuf::from("/r/cam1/main/2026-10-31/153558Z.idx")
        );
    }
}
