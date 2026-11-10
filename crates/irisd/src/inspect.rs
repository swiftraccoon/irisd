//! `irisd inspect`: scan a recording and report its structure and timing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_core::time::{MediaTicks, TIMESCALE};
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_file_bounded};
use iris_mp4::reader::{self, FragmentInfo, ScanOptions};
use iris_store::layout;
use iris_store::open_regular_file;

/// Arguments for `inspect`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The recording (`.mp4`) to scan.
    file: PathBuf,
    /// Read every payload and verify fragment checksums (slower).
    #[arg(long)]
    verify: bool,
    /// Print one line per fragment.
    #[arg(long)]
    samples: bool,
}

/// Runs the subcommand: the report goes to standard output.
pub fn run(args: &Args) -> anyhow::Result<()> {
    write_report(args, &mut std::io::stdout().lock())
}

/// Scans the recording and writes the report to `out`.
fn write_report(args: &Args, out: &mut impl Write) -> anyhow::Result<()> {
    let file =
        File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("reading the status of {}", args.file.display()))?
        .len();
    let options = ScanOptions {
        verify_crc: args.verify,
        collect_samples: true,
    };
    let report = reader::scan(BufReader::new(file), options)
        .with_context(|| format!("scanning {}", args.file.display()))?;
    let fragments = &report.fragments;

    writeln!(
        out,
        "file:               {} ({file_len} bytes)",
        args.file.display()
    )?;
    writeln!(out, "init segment:       {} bytes", report.init_len)?;
    let trailing = if report.valid_len < file_len {
        " (file has trailing data)"
    } else {
        ""
    };
    writeln!(
        out,
        "valid length:       {} bytes{trailing}",
        report.valid_len
    )?;
    writeln!(out, "fragments:          {}", fragments.len())?;
    writeln!(out, "crc verified:       {}", args.verify)?;

    if let (Some(first), Some(last)) = (fragments.first(), fragments.last()) {
        report_timing(out, fragments, first, last, report.valid_len)?;
    }
    match &report.stop {
        Some(stop) => writeln!(out, "scan stopped:       {stop}")?,
        None => writeln!(out, "scan stopped:       clean end of file")?,
    }
    writeln!(
        out,
        "sidecar index:      {}",
        sidecar_report(args, fragments)
    )?;
    if args.samples {
        print_fragments(out, fragments)?;
    }
    Ok(())
}

fn report_timing(
    out: &mut impl Write,
    fragments: &[FragmentInfo],
    first: &FragmentInfo,
    last: &FragmentInfo,
    valid_len: u64,
) -> anyhow::Result<()> {
    let end_time = last
        .decode_time
        .checked_add(u64::from(last.duration))
        .context("invalid media timeline: final decode-time end overflows u64")?;
    let total_ticks = end_time
        .checked_sub(first.decode_time)
        .context("invalid media timeline: final end precedes first decode time")?;
    let total_secs = total_ticks as f64 / f64::from(TIMESCALE);
    writeln!(
        out,
        "decode time:        {} .. {} ticks ({total_secs:.3} s)",
        first.decode_time, end_time
    )?;

    let sample_count: u64 = fragments.iter().map(|f| u64::from(f.sample_count)).sum();
    let payload: u64 = fragments
        .iter()
        .flat_map(|f| f.sample_sizes.iter())
        .map(|s| u64::from(*s))
        .sum();
    writeln!(
        out,
        "samples:            {sample_count} ({payload} payload bytes)"
    )?;
    if payload > 0 {
        let overhead = valid_len.saturating_sub(payload);
        writeln!(
            out,
            "container overhead: {:.4} % ({overhead} bytes including the init segment)",
            overhead as f64 / payload as f64 * 100.0
        )?;
        if total_secs > 0.0 {
            writeln!(
                out,
                "payload bitrate:    {:.3} Mbps",
                payload as f64 * 8.0 / total_secs / 1e6
            )?;
            writeln!(
                out,
                "mean frame rate:    {:.3} fps",
                sample_count as f64 / total_secs
            )?;
        }
    }

    let sequence_gaps = fragments
        .windows(2)
        .filter(|w| w[1].sequence != w[0].sequence.wrapping_add(1))
        .count();
    let decode_gaps = fragments
        .windows(2)
        .filter(|w| {
            w[0].decode_time.checked_add(u64::from(w[0].duration)) != Some(w[1].decode_time)
        })
        .count();
    if fragments
        .windows(2)
        .any(|window| window[1].decode_time < window[0].decode_time)
    {
        bail!("invalid media timeline: decode time decreases between fragments");
    }
    writeln!(out, "sequence gaps:      {sequence_gaps}")?;
    writeln!(out, "decode-time gaps:   {decode_gaps}")?;
    let sync_starts = fragments.iter().filter(|f| f.starts_with_sync).count();
    writeln!(
        out,
        "sync-start frags:   {sync_starts} of {}",
        fragments.len()
    )?;

    let mut histogram: HashMap<u32, usize> = HashMap::new();
    for duration in fragments.iter().flat_map(|f| f.sample_durations.iter()) {
        *histogram.entry(*duration).or_default() += 1;
    }
    let mut top: Vec<(u32, usize)> = histogram.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let ticks_per_ms = f64::from(TIMESCALE) / 1_000.0;
    let summary: Vec<String> = top
        .iter()
        .take(5)
        .map(|(ticks, count)| {
            format!(
                "{ticks} ticks ({:.2} ms) x{count}",
                f64::from(*ticks) / ticks_per_ms
            )
        })
        .collect();
    writeln!(out, "sample durations:   {}", summary.join(", "))?;

    report_wall_clock(out, first, last)
}

/// Writes the wall-clock span and its drift against media time, when the
/// fragments carry wall times.
fn report_wall_clock(
    out: &mut impl Write,
    first: &FragmentInfo,
    last: &FragmentInfo,
) -> anyhow::Result<()> {
    match (first.wall, last.wall) {
        (Some(first_wall), Some(last_wall)) => {
            let wall_delta = last_wall
                .get()
                .checked_sub(first_wall.get())
                .context("invalid media timeline: wall-clock delta overflows i64")?;
            let media_ticks = last
                .decode_time
                .checked_sub(first.decode_time)
                .context("invalid media timeline: final decode time precedes first")?;
            let media_ticks = i64::try_from(media_ticks)
                .context("invalid media timeline: decode-time delta exceeds i64")?;
            let media_delta = MediaTicks::new(media_ticks)
                .checked_as_micros()
                .context("invalid media timeline: media delta in microseconds exceeds i64")?;
            let drift = wall_delta
                .checked_sub(media_delta)
                .context("invalid media timeline: wall/media drift overflows i64")?;
            writeln!(
                out,
                "wall clock:         {first_wall} .. {last_wall}; drift vs media time {:+.1} ms",
                drift as f64 / 1000.0
            )?;
        }
        _ => writeln!(out, "wall clock:         no prft boxes")?,
    }
    Ok(())
}

/// Describes the sidecar beside the recording. It is opened without following
/// a final symlink or blocking on a FIFO, and read within the shared record
/// limit.
fn sidecar_report(args: &Args, fragments: &[FragmentInfo]) -> String {
    let path = layout::index_path(&args.file);
    let index = match open_regular_file(&path) {
        Ok(index) => index,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return format!("none at {}", path.display());
        }
        Err(error) => return format!("unusable: {error}"),
    };
    match read_index_file_bounded(index, MAX_PERSISTED_INDEX_RECORDS) {
        Ok((header, records)) => {
            let offsets_agree = records.len() == fragments.len()
                && records.iter().zip(fragments).all(|(r, f)| {
                    r.offset == f.offset && r.len == f.len && r.decode_time == f.decode_time
                });
            format!(
                "{} records (scanned {} fragments); init_len {}; run {}; offsets {}",
                records.len(),
                fragments.len(),
                header.init_len,
                header.run_id,
                if offsets_agree { "agree" } else { "DISAGREE" }
            )
        }
        Err(err) => format!("unreadable: {err}"),
    }
}

fn print_fragments(out: &mut impl Write, fragments: &[FragmentInfo]) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(
        out,
        "{:>6} {:>12} {:>9} {:>12} {:>7} {:>5} {:>4}  wall",
        "seq", "offset", "len", "decode", "dur", "n", "sync"
    )?;
    for f in fragments {
        writeln!(
            out,
            "{:>6} {:>12} {:>9} {:>12} {:>7} {:>5} {:>4}  {}",
            f.sequence,
            f.offset,
            f.len,
            f.decode_time,
            f.duration,
            f.sample_count,
            if f.starts_with_sync { "yes" } else { "no" },
            f.wall.map_or_else(|| "-".to_owned(), |w| w.to_string())
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use iris_core::time::WallUs;

    use super::*;

    fn fragment(decode_time: u64, duration: u32, wall: Option<WallUs>) -> FragmentInfo {
        FragmentInfo {
            offset: 700,
            len: 100,
            payload_offset: 740,
            sequence: 1,
            decode_time,
            duration,
            sample_count: 1,
            starts_with_sync: true,
            wall,
            sample_durations: vec![duration],
            sample_sizes: vec![1],
        }
    }

    /// A recording of three fragments of two samples, the third after a
    /// skipped sequence number and a 3,000-tick gap, starting on a dependent
    /// sample; with the length of its initialization section.
    fn gapped_recording() -> (Vec<u8>, usize) {
        use bytes::Bytes;
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        use iris_mp4::init::{SampleEntry, build_init_segment};

        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        );
        let mut recording = build_init_segment(&entry).to_vec();
        let init_len = recording.len();
        let start = 1_700_000_000_000_000;
        for (sequence, decode_time, wall, sync) in [
            (1, 0, start, true),
            (2, 6_000, start + 66_667, true),
            (4, 15_000, start + 200_000, false),
        ] {
            let samples = [
                Sample {
                    data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                    duration: 3_000,
                    is_sync: sync,
                },
                Sample {
                    data: Bytes::from_static(&[0, 0, 0, 3, 0x02, 1, 7]),
                    duration: 3_000,
                    is_sync: false,
                },
            ];
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence,
                    decode_time,
                    wall: WallUs::new(wall),
                },
                &samples,
            )
            .unwrap();
            for slice in encoded.io_slices(&samples) {
                recording.extend_from_slice(&slice);
            }
        }
        (recording, init_len)
    }

    #[test]
    fn the_report_states_the_structure_timing_and_gaps_of_a_recording() {
        let (recording, init_len) = gapped_recording();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recording.mp4");
        std::fs::write(&file, &recording).unwrap();
        let mut out = Vec::new();
        write_report(
            &Args {
                file: file.clone(),
                verify: true,
                samples: true,
            },
            &mut out,
        )
        .unwrap();
        let report = String::from_utf8(out).unwrap();
        let len = recording.len();
        // Everything but the 39 payload bytes is container.
        let overhead = len - 39;
        let overhead_percent = overhead as f64 / 39.0 * 100.0;
        for line in [
            format!("file:               {} ({len} bytes)", file.display()),
            format!("init segment:       {init_len} bytes"),
            format!("valid length:       {len} bytes"),
            "fragments:          3".to_owned(),
            "crc verified:       true".to_owned(),
            "decode time:        0 .. 21000 ticks (0.233 s)".to_owned(),
            "samples:            6 (39 payload bytes)".to_owned(),
            format!(
                "container overhead: {overhead_percent:.4} % ({overhead} bytes including the \
                 init segment)"
            ),
            // 312 bits and six samples over 21,000 ticks.
            "payload bitrate:    0.001 Mbps".to_owned(),
            "mean frame rate:    25.714 fps".to_owned(),
            "sequence gaps:      1".to_owned(),
            "decode-time gaps:   1".to_owned(),
            "sync-start frags:   2 of 3".to_owned(),
            "sample durations:   3000 ticks (33.33 ms) x6".to_owned(),
            // 200,000 us of wall clock over 15,000 ticks (166,666 us).
            "wall clock:         2023-11-14T22:13:20.000000Z .. 2023-11-14T22:13:20.200000Z; \
             drift vs media time +33.3 ms"
                .to_owned(),
            "scan stopped:       clean end of file".to_owned(),
            format!(
                "sidecar index:      none at {}",
                file.with_extension("idx").display()
            ),
            "   seq       offset       len       decode     dur     n sync  wall".to_owned(),
        ] {
            assert!(
                report.lines().any(|reported| reported == line),
                "missing {line:?} in\n{report}"
            );
        }
        let rows: Vec<&str> = report.lines().rev().take(3).collect();
        assert!(rows[0].starts_with("     4 "), "{report}");
        assert!(rows[0].contains("    15000    6000     2   no  2023-11-14T22:13:20.200000Z"));
        assert!(rows[2].contains("        0    6000     2  yes  2023-11-14T22:13:20.000000Z"));
    }

    /// Trailing bytes after the last fragment are named and not counted.
    #[test]
    fn the_report_names_trailing_data() {
        let (recording, _) = gapped_recording();
        let len = recording.len();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recording.mp4");
        let mut torn = recording.clone();
        torn.extend_from_slice(&[0; 5]);
        std::fs::write(&file, &torn).unwrap();
        let mut out = Vec::new();
        write_report(
            &Args {
                file,
                verify: false,
                samples: false,
            },
            &mut out,
        )
        .unwrap();
        let report = String::from_utf8(out).unwrap();
        assert!(
            report.contains(&format!(
                "valid length:       {len} bytes (file has trailing data)\n"
            )),
            "{report}"
        );
        assert!(report.contains("crc verified:       false\n"), "{report}");
        assert!(!report.contains("   seq "), "{report}");
    }

    #[test]
    fn timing_report_rejects_maximum_decreasing_and_unrepresentable_deltas() {
        let maximum = fragment(u64::MAX, 1, None);
        let error = report_timing(
            &mut Vec::new(),
            std::slice::from_ref(&maximum),
            &maximum,
            &maximum,
            800,
        )
        .unwrap_err();
        assert!(error.to_string().contains("overflows u64"), "{error:#}");

        let decreasing = [fragment(3_000, 1, None), fragment(0, 1, None)];
        let error = report_timing(
            &mut Vec::new(),
            &decreasing,
            &decreasing[0],
            &decreasing[1],
            900,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("precedes first decode time"),
            "{error:#}"
        );

        let too_wide = [
            fragment(0, 1, Some(WallUs::new(0))),
            fragment(i64::MAX as u64 + 1, 0, Some(WallUs::new(0))),
        ];
        let error =
            report_timing(&mut Vec::new(), &too_wide, &too_wide[0], &too_wide[1], 900).unwrap_err();
        assert!(error.to_string().contains("exceeds i64"), "{error:#}");
    }

    #[test]
    fn timing_report_accepts_the_largest_convertible_delta_and_ordinary_media() {
        let largest_convertible = u64::try_from(i128::from(i64::MAX) * 9 / 100).unwrap();
        let boundary = [
            fragment(0, 1, Some(WallUs::new(0))),
            fragment(largest_convertible, 0, Some(WallUs::new(i64::MAX))),
        ];
        report_timing(&mut Vec::new(), &boundary, &boundary[0], &boundary[1], 900).unwrap();

        let ordinary = [
            fragment(90_000, 90_000, Some(WallUs::new(1_000_000))),
            fragment(180_000, 90_000, Some(WallUs::new(2_000_000))),
        ];
        report_timing(&mut Vec::new(), &ordinary, &ordinary[0], &ordinary[1], 900).unwrap();
    }

    #[test]
    fn an_oversized_sidecar_is_reported_unreadable_without_loading_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let sidecar = File::create(layout::index_path(&media)).unwrap();
        let oversized = iris_mp4::index::HEADER_LEN
            + MAX_PERSISTED_INDEX_RECORDS * iris_mp4::index::RECORD_LEN
            + iris_mp4::index::RECORD_LEN;
        sidecar.set_len(u64::try_from(oversized).unwrap()).unwrap();
        let args = Args {
            file: media,
            verify: false,
            samples: false,
        };
        let report = sidecar_report(&args, &[]);
        assert!(report.starts_with("unreadable: "), "{report}");
    }

    #[test]
    fn a_missing_sidecar_is_reported_with_its_expected_path() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let args = Args {
            file: media.clone(),
            verify: false,
            samples: false,
        };
        assert_eq!(
            sidecar_report(&args, &[]),
            format!("none at {}", layout::index_path(&media).display())
        );
    }
}
