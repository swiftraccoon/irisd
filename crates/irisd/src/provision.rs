//! The cameras the API added at run time.
//!
//! `cameras.toml` in the state directory holds one `[[camera]]` table per
//! camera the API added (`[provisioning]` in the configuration), with the same
//! fields and validation as a configured camera. The daemon reads it after the
//! configuration at startup and rewrites it whole, through a private temporary
//! file renamed into place, after every addition or removal, so a crash leaves
//! the previous complete file or the rewritten one. Configured cameras never
//! appear in it; an entry whose id a configured camera also uses is dropped
//! with a warning when the file is read.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context, bail};
use iris_core::config::CameraConfig;
use iris_core::id::CameraId;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::permissions::{PROVISIONED_CAMERAS_FILE, open_private_file};

/// Longest store read; a few dozen cameras take a few kilobytes.
const MAX_STORE_BYTES: u64 = 1 << 20;

/// The file's shape: the configuration's `[[camera]]` array on its own.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Store {
    #[serde(default, rename = "camera")]
    cameras: Vec<CameraConfig>,
}

/// Reads the provisioned cameras in `dir`, none when the file is absent.
/// Every entry is validated like a configured camera and its stream URLs must
/// parse; an entry that fails, repeats an id, or uses one of the `configured`
/// ids is dropped with a warning rather than failing startup, since the
/// daemon can still record the others.
///
/// # Errors
///
/// Fails when the file is a link or a special file, cannot be read, is
/// larger than 1 MiB, or is not valid TOML of the store's shape. The error
/// names the file and a byte location, never the file's text, which holds
/// camera passwords.
pub fn load(dir: &Path, configured: &[CameraId]) -> anyhow::Result<Vec<CameraConfig>> {
    let path = dir.join(PROVISIONED_CAMERAS_FILE);
    // The store is read before the permission repair, so never through a link.
    let Some(mut file) = open_private_file(&path)? else {
        return Ok(Vec::new());
    };
    let mut text = String::new();
    (&mut file)
        .take(MAX_STORE_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.len() > usize::try_from(MAX_STORE_BYTES).unwrap_or(usize::MAX) {
        bail!(
            "{} holds more than {MAX_STORE_BYTES} bytes; the provisioned camera store is read up \
             to that size",
            path.display()
        );
    }
    let store: Store = toml::from_str(&text).map_err(|error| {
        anyhow::anyhow!(
            "{} is not a valid camera store ({})",
            path.display(),
            describe_span(error.span())
        )
    })?;
    let mut cameras = Vec::with_capacity(store.cameras.len());
    let mut seen: Vec<&CameraId> = Vec::new();
    for camera in &store.cameras {
        if configured.contains(&camera.id) || seen.contains(&&camera.id) {
            warn!(camera = %camera.id, "provisioned camera repeats a camera id; dropped");
            continue;
        }
        if let Err(error) = camera.validate() {
            warn!(camera = %camera.id, %error, "provisioned camera is invalid; dropped");
            continue;
        }
        if let Err(error) = crate::run::parsed_sources(camera) {
            warn!(
                camera = %camera.id,
                error = format_args!("{error:#}"),
                "provisioned camera has an unparsable stream URL; dropped"
            );
            continue;
        }
        seen.push(&camera.id);
        cameras.push(camera.clone());
    }
    Ok(cameras)
}

/// Where a parse error happened, without any of the file's text.
fn describe_span(span: Option<Range<usize>>) -> String {
    match span {
        Some(span) => format!(
            "invalid syntax or schema at bytes {}..{}",
            span.start, span.end
        ),
        None => "invalid syntax or schema at an unknown location".to_owned(),
    }
}

/// Writes `cameras` as the whole store in `dir`: a private temporary file,
/// synced and renamed over the store, then the directory synced. The two
/// syncs block; the daemon runs this off its runtime thread.
///
/// # Errors
///
/// Fails when the cameras cannot be encoded or any step of the write,
/// sync, or rename fails; the previous store stays in place.
pub fn save(dir: &Path, cameras: &[CameraConfig]) -> anyhow::Result<()> {
    let path = dir.join(PROVISIONED_CAMERAS_FILE);
    let temporary = dir.join(format!("{PROVISIONED_CAMERAS_FILE}.tmp"));
    let store = Store {
        cameras: cameras.to_vec(),
    };
    let text = toml::to_string(&store).context("encoding the provisioned cameras")?;
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("removing the stale {}", temporary.display()));
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, &path).with_context(|| format!("publishing {}", path.display()))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing the state directory {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use iris_core::config::{SnapshotMethod, TimestampMode};
    use iris_core::id::StreamKind;

    use super::*;

    fn camera(id: &str, url: &str) -> CameraConfig {
        CameraConfig {
            id: CameraId::try_new(id).unwrap(),
            name: format!("Camera {id}"),
            rtsp_url: url.to_owned(),
            sub_url: None,
            username: "user".to_owned(),
            password: "secret".to_owned(),
            vendor: None,
            record: vec![StreamKind::Main],
            timestamp_mode: TimestampMode::default(),
            http_port: None,
            https: false,
            tls_verify: true,
            retention_days: None,
            events: Vec::new(),
            snapshot: SnapshotMethod::Auto,
        }
    }

    #[test]
    fn a_missing_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), &[]).unwrap().is_empty());
    }

    #[test]
    fn a_saved_store_is_private_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        save(
            dir.path(),
            &[camera("added1", "rtsp://192.0.2.10:554/stream")],
        )
        .unwrap();
        let path = dir.path().join(PROVISIONED_CAMERAS_FILE);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dir.path().join("cameras.toml.tmp").exists());
    }

    #[test]
    fn a_saved_store_reads_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let cameras = vec![
            camera("added1", "rtsp://192.0.2.10:554/stream"),
            camera("added2", "rtsp://192.0.2.11/stream"),
        ];
        save(dir.path(), &cameras).unwrap();
        let loaded = load(dir.path(), &[]).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id.as_str(), "added1");
        assert_eq!(loaded[1].rtsp_url, "rtsp://192.0.2.11/stream");
        assert_eq!(loaded[1].password, "secret");
        save(dir.path(), &[]).unwrap();
        assert!(load(dir.path(), &[]).unwrap().is_empty());
    }

    #[test]
    fn entries_that_fail_admission_are_dropped_with_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cameras = vec![
            camera("static1", "rtsp://192.0.2.10/a"),
            camera("added1", "rtsp://192.0.2.11/a"),
            camera("added1", "rtsp://192.0.2.12/a"),
            camera("broken", "http://192.0.2.13/not-rtsp"),
            camera("badport", "rtsp://192.0.2.14:99999/a"),
            camera("added2", "rtsp://192.0.2.15/a"),
        ];
        save(dir.path(), &cameras).unwrap();
        let configured = [CameraId::try_new("static1").unwrap()];
        let loaded = load(dir.path(), &configured).unwrap();
        let ids: Vec<&str> = loaded.iter().map(|camera| camera.id.as_str()).collect();
        assert_eq!(ids, ["added1", "added2"]);
    }

    #[test]
    fn a_malformed_store_fails_naming_the_file_without_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROVISIONED_CAMERAS_FILE);
        let marker = "store-marker-password";
        for text in [
            format!("[[camera]]\nid = \"cam\"\npassword = \"{marker}\nrtsp_url = \"x\"\n"),
            format!("[[camera]]\nid = \"cam\"\npassword = \"{marker}\"\nrecord = \"main\"\n"),
            "not toml at all [[[".to_owned(),
        ] {
            std::fs::write(&path, &text).unwrap();
            let error = load(dir.path(), &[]).unwrap_err();
            let rendered = format!("{error:#} {error:?}");
            assert!(rendered.contains(PROVISIONED_CAMERAS_FILE), "{rendered}");
            assert!(!rendered.contains(marker), "{rendered}");
            assert!(rendered.contains("invalid syntax or schema"), "{rendered}");
        }
    }

    #[test]
    fn an_oversized_store_fails_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROVISIONED_CAMERAS_FILE);
        std::fs::write(
            &path,
            vec![b'#'; usize::try_from(MAX_STORE_BYTES).unwrap() + 1],
        )
        .unwrap();
        let error = load(dir.path(), &[]).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(PROVISIONED_CAMERAS_FILE), "{text}");
        assert!(text.contains("bytes"), "{text}");
    }
}
