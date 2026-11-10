//! `irisd hls`: static byte-range HLS playlists for one recording.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_core::time::TIMESCALE;
use iris_mp4::hls::{self as playlist, PlaylistOptions};
use iris_mp4::index::{IndexRecord, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media};
use iris_mp4::reader::{self, ScanOptions, ScanStop};
use iris_store::layout;
use iris_store::open_regular_file;
use tracing::warn;

const MEDIA_PLAYLIST: &str = "index.m3u8";
const MASTER_PLAYLIST: &str = "master.m3u8";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Arguments for `hls`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The recording (`.mp4`).
    file: PathBuf,
    /// Output directory for `master.m3u8`, `index.m3u8`, and a link to the
    /// recording.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Fragments (GOPs) per segment, at least one; grouping starts at sync
    /// fragments.
    #[arg(long, default_value = "3")]
    fragments_per_segment: NonZeroUsize,
    /// Make every segment exactly one fragment with its trailer excluded from
    /// the range.
    #[arg(long)]
    exclude_trailers: bool,
    /// RFC 6381 codecs string for the master playlist.
    #[arg(long, default_value = "hvc1.1.6.L150.B0")]
    codecs: String,
    /// Video resolution for the master playlist, as `WIDTHxHEIGHT`.
    #[arg(long, default_value = "3840x2160", value_parser = parse_resolution)]
    resolution: (u16, u16),
}

fn parse_resolution(text: &str) -> Result<(u16, u16), String> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {text:?}"))?;
    let width = w.parse().map_err(|_| format!("bad width in {text:?}"))?;
    let height = h.parse().map_err(|_| format!("bad height in {text:?}"))?;
    Ok((width, height))
}

/// Runs the subcommand.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let file_name = args
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .context("recording path has no UTF-8 file name")?;
    if matches!(file_name.as_str(), MEDIA_PLAYLIST | MASTER_PLAYLIST) {
        bail!("recording basename {file_name:?} is reserved for an HLS playlist");
    }
    let codecs = normalize_codecs(&args.codecs)?;
    let (init_len, records) = load_records(&args.file)?;
    if records.is_empty() {
        bail!("{} has no complete fragments", args.file.display());
    }

    fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    reject_generated_aliases(&args.file, &args.out)?;
    link_recording(&args.file, &args.out.join(&file_name))?;

    let duration_secs: f64 =
        records.iter().map(|r| f64::from(r.duration)).sum::<f64>() / f64::from(TIMESCALE);
    let options = PlaylistOptions {
        media_uri: encode_uri_segment(&file_name),
        fragments_per_segment: args.fragments_per_segment.get(),
        include_trailers: !args.exclude_trailers,
    };
    let rates = playlist::variant_bit_rates(&records, &options);
    let media = playlist::media_playlist(init_len, &records, &options)
        .context("the media file name cannot be written into a playlist")?;
    let master = playlist::master_playlist(
        MEDIA_PLAYLIST,
        &codecs,
        rates,
        args.resolution.0,
        args.resolution.1,
    )
    .context("the codecs string cannot be written into a playlist")?;
    publish_playlists(&args.out, &media, &master)?;

    let segments = records.len().div_ceil(if args.exclude_trailers {
        1
    } else {
        args.fragments_per_segment.get()
    });
    println!(
        "wrote {}/master.m3u8 and index.m3u8: {} fragments, about {segments} segments, \
         {duration_secs:.1} s, {} bps peak, {} bps mean",
        args.out.display(),
        records.len(),
        rates.peak,
        rates.average
    );
    println!("serve it with: irisd serve --dir {}", args.out.display());
    Ok(())
}

fn load_records(file: &Path) -> anyhow::Result<(u32, Vec<IndexRecord>)> {
    // Media is authoritative. Scanning every fragment binds every range,
    // duration and sync fact emitted into the playlist to this physical file.
    let mut media = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let media_len = media
        .metadata()
        .with_context(|| format!("reading the status of {}", file.display()))?
        .len();
    let report = reader::scan(
        BufReader::new(&mut media),
        ScanOptions {
            verify_crc: false,
            collect_samples: false,
        },
    )
    .with_context(|| format!("scanning {}", file.display()))?;
    if let Some(ScanStop::FragmentLimit { limit, .. }) = report.stop {
        bail!(
            "{} holds more than {limit} fragments, the most one playlist may describe",
            file.display()
        );
    }
    let records: Vec<_> = report
        .fragments
        .iter()
        .map(IndexRecord::from_scan)
        .collect();
    // Every playlist fact comes from the scan. The sidecar is read only to
    // report one that disagrees with the media: it is opened without
    // following a final symlink or blocking on a FIFO, and a failure is a
    // warning.
    let sidecar = layout::index_path(file);
    match open_regular_file(&sidecar) {
        Ok(index) => match read_index_file_bound_to_media(
            index,
            MAX_PERSISTED_INDEX_RECORDS,
            &report,
            media_len,
        ) {
            Ok(Some(_)) => {}
            Ok(None) => {
                warn!(
                    path = %sidecar.display(),
                    "sidecar index disagrees with the media; the playlists describe the media"
                );
            }
            Err(error) => {
                warn!(
                    path = %sidecar.display(),
                    %error,
                    "sidecar index is unreadable or over the limit; the playlists describe the \
                     media"
                );
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!(
                path = %sidecar.display(),
                %error,
                "sidecar index cannot be opened; the playlists describe the media"
            );
        }
    }
    Ok((report.init_len, records))
}

fn normalize_codecs(value: &str) -> anyhow::Result<String> {
    let mut normalized = Vec::new();
    for token in value.split(',') {
        let token = token.trim_matches(' ');
        if token.is_empty()
            || !token
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !token
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            || token.split('.').any(str::is_empty)
            || !token
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            bail!("invalid RFC 6381 codec token {token:?}");
        }
        normalized.push(token);
    }
    Ok(normalized.join(","))
}

fn encode_uri_segment(value: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Whether the entry at `path` is still the inode `claimed` holds open.
fn claimed_matches(path: &Path, claimed: &File) -> bool {
    claimed.metadata().is_ok_and(|identity| {
        fs::symlink_metadata(path).is_ok_and(|found| same_file(&identity, &found))
    })
}

fn pin_symlink(path: &Path) -> std::io::Result<File> {
    let claimed = rustix::fs::open(
        path,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    if claimed.metadata()?.file_type().is_symlink() {
        Ok(claimed)
    } else {
        Err(std::io::Error::other(
            "media-link temporary is not a symlink",
        ))
    }
}

fn remove_claimed_temporary(path: &Path, claimed: &File) {
    if claimed_matches(path, claimed) {
        let _ = fs::remove_file(path);
    }
}

fn reject_generated_aliases(source: &Path, out: &Path) -> anyhow::Result<()> {
    let source_meta = fs::metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?;
    for name in [MEDIA_PLAYLIST, MASTER_PLAYLIST] {
        let generated = out.join(name);
        match fs::metadata(&generated) {
            Ok(metadata) if same_file(&source_meta, &metadata) => {
                bail!(
                    "generated playlist {} aliases the input recording",
                    generated.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", generated.display()));
            }
        }
    }
    Ok(())
}

fn create_temp(out: &Path, destination: &str, contents: &[u8]) -> anyhow::Result<(PathBuf, File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = out.join(format!(
            ".{destination}.irisd-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(contents).and_then(|()| file.sync_data()) {
                    remove_claimed_temporary(&path, &file);
                    return Err(error).with_context(|| format!("writing {}", path.display()));
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!(
        "could not claim a unique playlist temporary in {}",
        out.display()
    )
}

fn publish_playlists(out: &Path, media: &str, master: &str) -> anyhow::Result<()> {
    publish_playlists_with(out, media, master, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationStep {
    BeforeMediaRename,
    AfterMediaRename,
    BeforeMasterRename,
    BeforeCommitSync,
}

fn publish_playlists_with(
    out: &Path,
    media: &str,
    master: &str,
    mut step: impl FnMut(PublicationStep) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    // Reject non-replaceable destinations before publishing either file.
    for destination in [MEDIA_PLAYLIST, MASTER_PLAYLIST] {
        match fs::symlink_metadata(out.join(destination)) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                bail!("playlist destination {destination} is a directory");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting {}", out.join(destination).display()));
            }
        }
    }
    let (media_temp, media_file) = create_temp(out, MEDIA_PLAYLIST, media.as_bytes())?;
    let (master_temp, master_file) = match create_temp(out, MASTER_PLAYLIST, master.as_bytes()) {
        Ok(value) => value,
        Err(error) => {
            remove_claimed_temporary(&media_temp, &media_file);
            return Err(error);
        }
    };
    let media_destination = out.join(MEDIA_PLAYLIST);
    let master_destination = out.join(MASTER_PLAYLIST);
    let mut media_temp_owned = true;
    let mut master_temp_owned = true;
    let result = (|| -> std::io::Result<()> {
        step(PublicationStep::BeforeMediaRename)?;
        if !claimed_matches(&media_temp, &media_file) {
            return Err(std::io::Error::other(
                "media playlist temporary ownership changed",
            ));
        }
        // `rename` replaces only the destination directory entry. It never
        // follows a symlink or writes through a hard link, and the prior entry
        // remains visible until this atomic replacement.
        fs::rename(&media_temp, &media_destination)?;
        media_temp_owned = false;
        step(PublicationStep::AfterMediaRename)?;
        step(PublicationStep::BeforeMasterRename)?;
        if !claimed_matches(&master_temp, &master_file) {
            return Err(std::io::Error::other(
                "master playlist temporary ownership changed",
            ));
        }
        fs::rename(&master_temp, &master_destination)?;
        master_temp_owned = false;
        step(PublicationStep::BeforeCommitSync)?;
        File::open(out)?.sync_all()
    })();
    if let Err(error) = result {
        // A completed rename already published a whole synced file and must
        // not be removed: every fixed destination remains its prior or replacement
        // valid entry, including when the second rename or final barrier
        // fails. Only invocation-owned temporaries are cleaned up.
        if media_temp_owned {
            remove_claimed_temporary(&media_temp, &media_file);
        }
        if master_temp_owned {
            remove_claimed_temporary(&master_temp, &master_file);
        }
        if let Err(sync_error) = File::open(out).and_then(|directory| directory.sync_all()) {
            warn!(
                path = %out.display(),
                %sync_error,
                "could not sync failed playlist publication state"
            );
        }
        return Err(error).with_context(|| format!("publishing playlists in {}", out.display()));
    }
    Ok(())
}

fn link_recording(source: &Path, link: &Path) -> anyhow::Result<()> {
    link_recording_with(source, link, |_| Ok(()))
}

fn link_recording_with(
    source: &Path,
    link: &Path,
    mut before_rename: impl FnMut(&Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let target = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;
    // The output directory may be the source directory, or an alias of it.
    // Removing that destination would remove the recording itself.
    if link.canonicalize().is_ok_and(|path| path == target) {
        return Ok(());
    }
    // Only an absent entry or a symbolic link is replaced; anything else at
    // the link's name is someone else's file.
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "{} exists and is not a link to the recording",
            link.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", link.display()));
        }
    }
    let parent = link
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = link
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recording");
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.irisd-link-{}-{sequence}.tmp",
            std::process::id()
        ));
        match std::os::unix::fs::symlink(&target, &temporary) {
            Ok(()) => {
                // Keep the symlink inode pinned. Metadata alone is not an
                // identity claim: an unlinked inode can be reused immediately
                // for a replacement at the same pathname.
                let claimed = pin_symlink(&temporary)?;
                let result = before_rename(&temporary).and_then(|()| {
                    if !claimed_matches(&temporary, &claimed) {
                        return Err(std::io::Error::other(
                            "media-link temporary ownership changed",
                        ));
                    }
                    fs::rename(&temporary, link)
                });
                if let Err(error) = result {
                    if claimed_matches(&temporary, &claimed) {
                        let _ = fs::remove_file(&temporary);
                    }
                    return Err(error)
                        .with_context(|| format!("publishing media link {}", link.display()));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating media link {}", link.display()));
            }
        }
    }
    bail!(
        "could not claim a unique media-link temporary in {}",
        parent.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_facts_come_from_the_media_whatever_the_sidecar_holds() {
        use bytes::Bytes;
        use iris_core::time::WallUs;
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        use iris_mp4::index::IndexHeader;
        use iris_mp4::init::{SampleEntry, build_init_segment};
        use std::io::Cursor;

        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        );
        let mut file = build_init_segment(&entry).to_vec();
        let samples = [Sample {
            data: Bytes::from_static(&[0, 0, 0, 2, 38, 1]),
            duration: 90_000,
            is_sync: true,
        }];
        for sequence in 1..=2 {
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence,
                    decode_time: 0,
                    wall: WallUs::new(0),
                },
                &samples,
            )
            .unwrap();
            for slice in encoded.io_slices(&samples) {
                file.extend_from_slice(&slice);
            }
        }
        let report = reader::scan(Cursor::new(&file), ScanOptions::default()).unwrap();
        let expected: Vec<_> = report
            .fragments
            .iter()
            .map(IndexRecord::from_scan)
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        fs::write(&media, &file).unwrap();
        for (bad_len, bad_offset) in [
            (0, expected[0].offset),
            (1, expected[0].offset),
            (23, expected[0].offset),
            (24, expected[0].offset),
            (100, u64::MAX),
            (100, 1),
            (100, expected[0].offset + 1),
        ] {
            let mut records = expected.clone();
            let index = usize::from(bad_len == 100);
            records[index].len = bad_len;
            records[index].offset = bad_offset;
            let mut bytes = IndexHeader {
                init_len: report.init_len,
                ..IndexHeader::default()
            }
            .encode()
            .to_vec();
            for record in records {
                bytes.extend(record.encode());
            }
            fs::write(layout::index_path(&media), bytes).unwrap();
            let (init_len, loaded) = load_records(&media).unwrap();
            assert_eq!(init_len, report.init_len);
            assert_eq!(loaded, expected);
        }

        let sidecar = File::create(layout::index_path(&media)).unwrap();
        let oversized = iris_mp4::index::HEADER_LEN
            + MAX_PERSISTED_INDEX_RECORDS * iris_mp4::index::RECORD_LEN
            + iris_mp4::index::RECORD_LEN;
        sidecar.set_len(u64::try_from(oversized).unwrap()).unwrap();
        let (init_len, loaded) = load_records(&media).unwrap();
        assert_eq!(init_len, report.init_len);
        assert_eq!(loaded, expected);
    }

    /// A recording of two one-second fragments, each one sync sample.
    fn two_second_recording() -> Vec<u8> {
        use bytes::Bytes;
        use iris_core::time::WallUs;
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        use iris_mp4::init::{SampleEntry, build_init_segment};

        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        );
        let mut file = build_init_segment(&entry).to_vec();
        let samples = [Sample {
            data: Bytes::from_static(&[0, 0, 0, 2, 38, 1]),
            duration: 90_000,
            is_sync: true,
        }];
        for sequence in 1..=2u32 {
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence,
                    decode_time: u64::from(sequence - 1) * 90_000,
                    wall: WallUs::new(0),
                },
                &samples,
            )
            .unwrap();
            for slice in encoded.io_slices(&samples) {
                file.extend_from_slice(&slice);
            }
        }
        file
    }

    fn arguments(file: &Path, out: &Path) -> Args {
        Args {
            file: file.to_path_buf(),
            out: out.to_path_buf(),
            fragments_per_segment: NonZeroUsize::new(1).unwrap(),
            exclude_trailers: false,
            codecs: "hvc1.1.6.L150.B0".into(),
            resolution: (1920, 1080),
        }
    }

    /// The command end to end: the output directory holds the recording
    /// under its own name and two playlists that describe it.
    #[test]
    fn the_command_writes_both_playlists_and_links_the_recording() {
        let dir = tempfile::tempdir().unwrap();
        let recording = two_second_recording();
        let source = dir.path().join("front door.mp4");
        fs::write(&source, &recording).unwrap();
        let out = dir.path().join("site");
        run(&arguments(&source, &out)).unwrap();

        assert_eq!(fs::read(out.join("front door.mp4")).unwrap(), recording);
        let media = fs::read_to_string(out.join(MEDIA_PLAYLIST)).unwrap();
        assert!(media.starts_with("#EXTM3U\n"), "{media}");
        assert!(media.contains("#EXT-X-TARGETDURATION:1\n"), "{media}");
        assert!(
            media.contains("#EXT-X-MAP:URI=\"front%20door.mp4\",BYTERANGE=\""),
            "{media}"
        );
        assert_eq!(media.matches("#EXTINF:1.00000,\n").count(), 2, "{media}");
        assert_eq!(media.matches("\nfront%20door.mp4\n").count(), 2, "{media}");
        assert!(media.ends_with("#EXT-X-ENDLIST\n"), "{media}");
        let master = fs::read_to_string(out.join(MASTER_PLAYLIST)).unwrap();
        assert!(
            master.contains("CODECS=\"hvc1.1.6.L150.B0\",RESOLUTION=1920x1080\n"),
            "{master}"
        );
        assert!(master.ends_with(&format!("{MEDIA_PLAYLIST}\n")), "{master}");
        // Nothing else: no temporary file stays behind.
        assert_eq!(fs::read_dir(&out).unwrap().count(), 3);

        // A second run over the same output replaces the playlists.
        run(&arguments(&source, &out)).unwrap();
        assert_eq!(fs::read_dir(&out).unwrap().count(), 3);
        assert_eq!(fs::read_to_string(out.join(MEDIA_PLAYLIST)).unwrap(), media);
    }

    /// A playlist name in the output directory that is the recording itself
    /// (a hard link) would be overwritten by publication: the command
    /// refuses and the recording keeps its bytes.
    #[test]
    fn an_output_playlist_name_that_is_the_recording_is_refused() {
        for name in [MEDIA_PLAYLIST, MASTER_PLAYLIST] {
            let dir = tempfile::tempdir().unwrap();
            let recording = two_second_recording();
            let source = dir.path().join("recording.mp4");
            fs::write(&source, &recording).unwrap();
            let out = dir.path().join("site");
            fs::create_dir(&out).unwrap();
            fs::hard_link(&source, out.join(name)).unwrap();
            let error = run(&arguments(&source, &out)).unwrap_err();
            assert!(error.to_string().contains("aliases the input"), "{error:#}");
            assert_eq!(fs::read(&source).unwrap(), recording);
            assert_eq!(fs::read(out.join(name)).unwrap(), recording);
        }
    }

    #[test]
    fn a_recording_without_a_complete_fragment_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let recording = two_second_recording();
        let report =
            reader::scan(std::io::Cursor::new(&recording), ScanOptions::default()).unwrap();
        let source = dir.path().join("recording.mp4");
        // The initialization section and half of the first fragment.
        let cut = report.fragments[0].offset as usize + report.fragments[0].len as usize / 2;
        fs::write(&source, &recording[..cut]).unwrap();
        let out = dir.path().join("site");
        let error = run(&arguments(&source, &out)).unwrap_err();
        assert!(
            error.to_string().contains("no complete fragments"),
            "{error:#}"
        );
        assert!(!out.exists());
    }

    #[test]
    fn an_unrelated_file_at_the_link_path_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("recording.mp4");
        let output = dir.path().join("output");
        fs::create_dir(&output).unwrap();
        fs::write(&source, b"recording bytes").unwrap();
        let other = output.join("recording.mp4");
        fs::write(&other, b"someone else's recording").unwrap();
        let error = link_recording(&source, &other).unwrap_err();
        assert!(
            error.to_string().contains("is not a link to the recording"),
            "{error:#}"
        );
        assert_eq!(fs::read(&other).unwrap(), b"someone else's recording");
        assert!(
            !fs::symlink_metadata(&other)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        fs::remove_file(&other).unwrap();
        std::os::unix::fs::symlink(dir.path().join("stale"), &other).unwrap();
        link_recording(&source, &other).unwrap();
        assert_eq!(fs::read(&other).unwrap(), b"recording bytes");
    }

    #[test]
    fn linking_a_recording_to_itself_preserves_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        fs::write(&path, b"recorded video").unwrap();
        link_recording(&path, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"recorded video");
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_concurrently_recreated_media_link_is_replaced_without_following_its_victim() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("recording.mp4");
        let output = dir.path().join("output");
        let link = output.join("recording.mp4");
        let victim = dir.path().join("victim");
        fs::create_dir(&output).unwrap();
        fs::write(&source, b"recording bytes").unwrap();
        fs::write(&victim, b"victim sentinel").unwrap();

        link_recording_with(&source, &link, |_| {
            std::os::unix::fs::symlink(&victim, &link)
        })
        .unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"victim sentinel");
        assert_eq!(fs::read(&link).unwrap(), b"recording bytes");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            source.canonicalize().unwrap()
        );
        assert!(fs::read_dir(&output).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".irisd-link-")
        }));
    }

    #[test]
    fn media_link_never_publishes_or_removes_a_recreated_temporary_name() {
        use std::cell::RefCell;

        for callback_fails in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("recording.mp4");
            let output = dir.path().join("output");
            let link = output.join("recording.mp4");
            let victim = dir.path().join("victim");
            fs::create_dir(&output).unwrap();
            fs::write(&source, b"recording bytes").unwrap();
            fs::write(&victim, b"victim sentinel").unwrap();
            let recreated = RefCell::new(None);

            let error = link_recording_with(&source, &link, |temporary| {
                fs::remove_file(temporary)?;
                std::os::unix::fs::symlink(&victim, temporary)?;
                recreated.replace(Some(temporary.to_path_buf()));
                if callback_fails {
                    Err(std::io::Error::other("injected media-link failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            if callback_fails {
                assert!(format!("{error:#}").contains("injected media-link failure"));
            } else {
                assert!(format!("{error:#}").contains("temporary ownership changed"));
            }
            let recreated = recreated.into_inner().unwrap();
            assert_eq!(fs::read_link(recreated).unwrap(), victim);
            assert_eq!(fs::read(&victim).unwrap(), b"victim sentinel");
            assert!(!link.exists());
        }
    }

    #[test]
    fn reserved_playlist_basenames_are_rejected_before_any_output_mutation() {
        for name in [MEDIA_PLAYLIST, MASTER_PLAYLIST] {
            for relationship in ["same", "alias", "separate"] {
                let dir = tempfile::tempdir().unwrap();
                let source_dir = dir.path().join("source");
                fs::create_dir(&source_dir).unwrap();
                let source = source_dir.join(name);
                fs::write(&source, b"recording sentinel").unwrap();
                let before = fs::metadata(&source).unwrap();
                let out = match relationship {
                    "same" => source_dir.clone(),
                    "alias" => {
                        let alias = dir.path().join("source-alias");
                        std::os::unix::fs::symlink(&source_dir, &alias).unwrap();
                        alias
                    }
                    "separate" => dir.path().join("output"),
                    _ => unreachable!(),
                };
                let error = run(&Args {
                    file: source.clone(),
                    out: out.clone(),
                    fragments_per_segment: NonZeroUsize::new(3).unwrap(),
                    exclude_trailers: false,
                    codecs: "hvc1.1.6.L150.B0".into(),
                    resolution: (1920, 1080),
                })
                .unwrap_err();

                assert!(error.to_string().contains("reserved"), "{error:#}");
                assert_eq!(fs::read(&source).unwrap(), b"recording sentinel");
                assert!(same_file(&before, &fs::metadata(&source).unwrap()));
                if relationship == "separate" {
                    assert!(!out.exists());
                }
            }
        }
    }

    #[test]
    fn publication_atomically_replaces_prior_entries_without_an_absent_phase() {
        let dir = tempfile::tempdir().unwrap();
        let media_victim = dir.path().join("media-victim");
        let master_victim = dir.path().join("master-victim");
        fs::write(&media_victim, b"media sentinel").unwrap();
        fs::write(&master_victim, b"master sentinel").unwrap();
        let media_destination = dir.path().join(MEDIA_PLAYLIST);
        let master_destination = dir.path().join(MASTER_PLAYLIST);
        std::os::unix::fs::symlink(&media_victim, &media_destination).unwrap();
        fs::hard_link(&master_victim, &master_destination).unwrap();
        let master_identity = fs::metadata(&master_destination).unwrap();
        let mut observed = Vec::new();

        publish_playlists_with(
            dir.path(),
            "replacement media",
            "replacement master",
            |step| {
                assert!(media_destination.exists());
                assert!(master_destination.exists());
                match step {
                    PublicationStep::BeforeMediaRename => {
                        assert!(
                            fs::symlink_metadata(&media_destination)?
                                .file_type()
                                .is_symlink()
                        );
                        assert!(same_file(
                            &master_identity,
                            &fs::metadata(&master_destination)?
                        ));
                    }
                    PublicationStep::AfterMediaRename | PublicationStep::BeforeMasterRename => {
                        assert!(
                            !fs::symlink_metadata(&media_destination)?
                                .file_type()
                                .is_symlink()
                        );
                        assert_eq!(fs::read(&media_destination)?, b"replacement media");
                        assert!(same_file(
                            &master_identity,
                            &fs::metadata(&master_destination)?
                        ));
                    }
                    PublicationStep::BeforeCommitSync => {
                        assert_eq!(fs::read(&media_destination)?, b"replacement media");
                        assert_eq!(fs::read(&master_destination)?, b"replacement master");
                        assert!(!same_file(
                            &master_identity,
                            &fs::metadata(&master_destination)?
                        ));
                    }
                }
                observed.push(step);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            observed,
            [
                PublicationStep::BeforeMediaRename,
                PublicationStep::AfterMediaRename,
                PublicationStep::BeforeMasterRename,
                PublicationStep::BeforeCommitSync,
            ]
        );
        assert_eq!(fs::read(&media_victim).unwrap(), b"media sentinel");
        assert_eq!(fs::read(&master_victim).unwrap(), b"master sentinel");
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".irisd-")
        }));
    }

    #[test]
    fn failed_publication_keeps_destinations_valid_prior_or_replacement() {
        for failure in [
            PublicationStep::BeforeMediaRename,
            PublicationStep::AfterMediaRename,
            PublicationStep::BeforeMasterRename,
            PublicationStep::BeforeCommitSync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let media_victim = dir.path().join("media-victim");
            let master_victim = dir.path().join("master-victim");
            fs::write(&media_victim, b"media sentinel").unwrap();
            fs::write(&master_victim, b"master sentinel").unwrap();
            let media_destination = dir.path().join(MEDIA_PLAYLIST);
            let master_destination = dir.path().join(MASTER_PLAYLIST);
            std::os::unix::fs::symlink(&media_victim, &media_destination).unwrap();
            fs::hard_link(&master_victim, &master_destination).unwrap();
            let master_identity = fs::metadata(&master_destination).unwrap();

            let mut injected = false;
            let error = publish_playlists_with(
                dir.path(),
                "replacement media",
                "replacement master",
                |step| {
                    if step == failure {
                        injected = true;
                        Err(std::io::Error::other("injected publication failure"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert!(injected);
            assert!(error.to_string().contains("publishing playlists"));
            assert!(media_destination.exists());
            assert!(master_destination.exists());
            if failure == PublicationStep::BeforeMediaRename {
                assert!(
                    fs::symlink_metadata(&media_destination)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read_link(&media_destination).unwrap(), media_victim);
            } else {
                assert_eq!(fs::read(&media_destination).unwrap(), b"replacement media");
            }
            if failure == PublicationStep::BeforeCommitSync {
                assert_eq!(
                    fs::read(&master_destination).unwrap(),
                    b"replacement master"
                );
            } else {
                assert!(same_file(
                    &master_identity,
                    &fs::metadata(&master_destination).unwrap()
                ));
            }
            assert_eq!(fs::read(&media_victim).unwrap(), b"media sentinel");
            assert_eq!(fs::read(&master_victim).unwrap(), b"master sentinel");
            assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".irisd-")
            }));
        }
    }

    #[test]
    fn failed_fresh_publication_leaves_only_whole_files_and_no_temporary() {
        for failure in [
            PublicationStep::BeforeMediaRename,
            PublicationStep::AfterMediaRename,
            PublicationStep::BeforeMasterRename,
            PublicationStep::BeforeCommitSync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            assert!(
                publish_playlists_with(dir.path(), "media", "master", |step| {
                    if step == failure {
                        Err(std::io::Error::other("injected publication failure"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            let media = dir.path().join(MEDIA_PLAYLIST);
            let master = dir.path().join(MASTER_PLAYLIST);
            if failure == PublicationStep::BeforeMediaRename {
                assert!(!media.exists());
            } else {
                assert_eq!(fs::read(&media).unwrap(), b"media");
            }
            if failure == PublicationStep::BeforeCommitSync {
                assert_eq!(fs::read(&master).unwrap(), b"master");
            } else {
                assert!(!master.exists());
            }
            assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".irisd-")
            }));
        }
    }

    #[test]
    fn failed_commit_never_removes_recreated_former_temporary_names() {
        use std::cell::RefCell;

        let dir = tempfile::tempdir().unwrap();
        let temporaries = RefCell::new(Vec::new());
        let error = publish_playlists_with(dir.path(), "media", "master", |step| {
            match step {
                PublicationStep::BeforeMediaRename => {
                    temporaries.borrow_mut().extend(
                        fs::read_dir(dir.path())?
                            .map(|entry| entry.map(|entry| entry.path()))
                            .collect::<Result<Vec<_>, _>>()?
                            .into_iter()
                            .filter(|path| {
                                path.file_name()
                                    .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
                            }),
                    );
                }
                PublicationStep::BeforeCommitSync => {
                    assert_eq!(temporaries.borrow().len(), 2);
                    for path in temporaries.borrow().iter() {
                        fs::write(path, b"concurrent sentinel")?;
                    }
                    return Err(std::io::Error::other("injected commit failure"));
                }
                PublicationStep::AfterMediaRename | PublicationStep::BeforeMasterRename => {}
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected commit failure"));
        for path in temporaries.into_inner() {
            assert_eq!(fs::read(path).unwrap(), b"concurrent sentinel");
        }
    }

    #[test]
    fn pre_rename_failure_never_removes_a_recreated_temporary_name() {
        use std::cell::RefCell;

        let dir = tempfile::tempdir().unwrap();
        let recreated = RefCell::new(None);
        let error = publish_playlists_with(dir.path(), "media", "master", |step| {
            if step != PublicationStep::BeforeMediaRename {
                return Ok(());
            }
            let temporary = fs::read_dir(dir.path())?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .find(|path| {
                    path.file_name().is_some_and(|name| {
                        let name = name.to_string_lossy();
                        name.contains(MEDIA_PLAYLIST) && name.ends_with(".tmp")
                    })
                })
                .expect("media playlist temporary");
            fs::remove_file(&temporary)?;
            fs::write(&temporary, b"concurrent sentinel")?;
            recreated.replace(Some(temporary));
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("temporary ownership changed"));
        let recreated = recreated.into_inner().unwrap();
        assert_eq!(fs::read(recreated).unwrap(), b"concurrent sentinel");
    }

    #[test]
    fn parses_resolution() {
        assert_eq!(parse_resolution("1920x1080"), Ok((1920, 1080)));
        assert!(parse_resolution("1920").is_err());
        assert!(parse_resolution("ax1").is_err());
    }

    #[test]
    fn uri_segments_percent_encode_delimiters_controls_spaces_percent_and_unicode() {
        let encoded = encode_uri_segment("camera #\"%\r\n\u{1}雪.mp4");
        assert_eq!(encoded, "camera%20%23%22%25%0D%0A%01%E9%9B%AA.mp4");
        assert!(
            !encoded
                .bytes()
                .any(|byte| matches!(byte, b'#' | b'\"' | b'\r' | b'\n' | b' '))
        );

        let playlist = playlist::media_playlist(
            700,
            &[IndexRecord {
                offset: 700,
                len: 100,
                duration: 90_000,
                flags: iris_mp4::index::FLAG_STARTS_WITH_SYNC,
                ..IndexRecord::default()
            }],
            &PlaylistOptions {
                media_uri: encoded,
                fragments_per_segment: 1,
                include_trailers: true,
            },
        )
        .unwrap();
        assert_eq!(
            playlist
                .lines()
                .filter(|line| line.starts_with("#EXT-X-MAP:"))
                .count(),
            1
        );
        assert_eq!(
            playlist
                .lines()
                .filter(|line| !line.starts_with('#') && !line.is_empty())
                .count(),
            1
        );
    }

    #[test]
    fn codec_lists_normalize_valid_tokens_and_reject_line_or_attribute_injection() {
        assert_eq!(
            normalize_codecs("hvc1.1.6.L150.B0, mp4a.40.2").unwrap(),
            "hvc1.1.6.L150.B0,mp4a.40.2"
        );
        for invalid in [
            "",
            ",",
            "hvc1,,mp4a.40.2",
            ".hvc1",
            "hvc1.",
            "-hvc1",
            "hvc1-",
            "hvc1 token",
            "hvc1\t token",
            "hvc1\"",
            "hvc1\r",
            "hvc1\n#EXT-X-ENDLIST",
            "hvc1%0A",
            "雪",
        ] {
            assert!(normalize_codecs(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
