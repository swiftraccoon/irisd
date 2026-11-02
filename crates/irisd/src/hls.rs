//! `irisd hls`: static byte-range HLS playlists for one recording.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_core::time::TIMESCALE;
use iris_mp4::hls::{PlaylistOptions, master_playlist, media_playlist};
use iris_mp4::index::{IndexRecord, read_index};
use iris_mp4::reader::{ScanOptions, scan};
use iris_store::layout::index_path;
use tracing::warn;

/// Arguments for `hls`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The recording (`.mp4`).
    file: PathBuf,
    /// Output directory for `master.m3u8`, `index.m3u8`, and a link to the recording.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Fragments (GOPs) per segment; grouping starts at sync fragments.
    #[arg(long, default_value_t = 3)]
    fragments_per_segment: usize,
    /// Make every segment exactly one fragment with its trailer excluded from the range.
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
    let (init_len, records) = load_records(&args.file)?;
    if records.is_empty() {
        bail!("{} has no complete fragments", args.file.display());
    }
    let file_name = args
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .context("recording path has no file name")?;

    fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    link_recording(&args.file, &args.out.join(&file_name))?;

    let duration_secs: f64 =
        records.iter().map(|r| f64::from(r.duration)).sum::<f64>() / f64::from(TIMESCALE);
    let total_bytes: u64 =
        u64::from(init_len) + records.iter().map(|r| u64::from(r.len)).sum::<u64>();
    let bandwidth = if duration_secs > 0.0 {
        (total_bytes as f64 * 8.0 / duration_secs) as u64
    } else {
        0
    };

    let options = PlaylistOptions {
        media_uri: file_name,
        fragments_per_segment: args.fragments_per_segment,
        include_trailers: !args.exclude_trailers,
    };
    let media = media_playlist(init_len, &records, &options);
    let master = master_playlist(
        "index.m3u8",
        &args.codecs,
        bandwidth,
        args.resolution.0,
        args.resolution.1,
    );
    fs::write(args.out.join("index.m3u8"), media)?;
    fs::write(args.out.join("master.m3u8"), master)?;

    let segments = records.len().div_ceil(if args.exclude_trailers {
        1
    } else {
        args.fragments_per_segment.max(1)
    });
    println!(
        "wrote {}/master.m3u8 and index.m3u8: {} fragments, about {segments} segments, {duration_secs:.1} s, {bandwidth} bps",
        args.out.display(),
        records.len()
    );
    println!("serve it with: irisd serve --dir {}", args.out.display());
    Ok(())
}

fn load_records(file: &Path) -> anyhow::Result<(u32, Vec<IndexRecord>)> {
    let sidecar = index_path(file);
    if sidecar.exists() {
        match read_index(&sidecar) {
            Ok((header, records)) if !records.is_empty() => return Ok((header.init_len, records)),
            Ok(_) => {
                warn!(path = %sidecar.display(), "sidecar index is empty; scanning the recording");
            }
            Err(err) => {
                warn!(path = %sidecar.display(), %err, "sidecar index unreadable; scanning the recording");
            }
        }
    }
    let reader =
        BufReader::new(File::open(file).with_context(|| format!("opening {}", file.display()))?);
    let report = scan(reader, ScanOptions::default())?;
    let records = report
        .fragments
        .iter()
        .map(IndexRecord::from_scan)
        .collect();
    Ok((report.init_len, records))
}

fn link_recording(source: &Path, link: &Path) -> anyhow::Result<()> {
    let target = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;
    if link.exists() || fs::symlink_metadata(link).is_ok() {
        fs::remove_file(link)?;
    }
    if let Err(err) = std::os::unix::fs::symlink(&target, link) {
        warn!(%err, "symlink failed; copying the recording instead");
        fs::copy(&target, link).with_context(|| format!("copying to {}", link.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resolution() {
        assert_eq!(parse_resolution("1920x1080"), Ok((1920, 1080)));
        assert!(parse_resolution("1920").is_err());
        assert!(parse_resolution("ax1").is_err());
    }
}
