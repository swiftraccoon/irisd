//! `irisd inspect`: scan a recording and report its structure and timing.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args as ClapArgs;
use iris_core::time::{MediaTicks, TIMESCALE};
use iris_mp4::index::read_index;
use iris_mp4::reader::{FragmentInfo, ScanOptions, scan};
use iris_store::layout::index_path;

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

/// Runs the subcommand.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let file =
        File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let file_len = file.metadata()?.len();
    let options = ScanOptions {
        verify_crc: args.verify,
        collect_samples: true,
    };
    let report = scan(BufReader::new(file), options)
        .with_context(|| format!("scanning {}", args.file.display()))?;
    let fragments = &report.fragments;

    println!(
        "file:               {} ({file_len} bytes)",
        args.file.display()
    );
    println!("init segment:       {} bytes", report.init_len);
    let trailing = if report.valid_len < file_len {
        " (file has trailing data)"
    } else {
        ""
    };
    println!("valid length:       {} bytes{trailing}", report.valid_len);
    println!("fragments:          {}", fragments.len());
    println!("crc verified:       {}", args.verify);

    if let (Some(first), Some(last)) = (fragments.first(), fragments.last()) {
        report_timing(fragments, first, last, report.valid_len);
    }
    match &report.stop {
        Some(stop) => println!("scan stopped:       {stop:?}"),
        None => println!("scan stopped:       clean end of file"),
    }
    report_sidecar(args, fragments);
    if args.samples {
        print_fragments(fragments);
    }
    Ok(())
}

fn report_timing(
    fragments: &[FragmentInfo],
    first: &FragmentInfo,
    last: &FragmentInfo,
    valid_len: u64,
) {
    let end_time = last.decode_time + u64::from(last.duration);
    let total_ticks = end_time.saturating_sub(first.decode_time);
    let total_secs = total_ticks as f64 / f64::from(TIMESCALE);
    println!(
        "decode time:        {} .. {} ticks ({total_secs:.3} s)",
        first.decode_time, end_time
    );

    let sample_count: u64 = fragments.iter().map(|f| u64::from(f.sample_count)).sum();
    let payload: u64 = fragments
        .iter()
        .flat_map(|f| f.sample_sizes.iter())
        .map(|s| u64::from(*s))
        .sum();
    println!("samples:            {sample_count} ({payload} payload bytes)");
    if payload > 0 {
        let overhead = valid_len.saturating_sub(payload);
        println!(
            "container overhead: {:.4} % ({overhead} bytes including the init segment)",
            overhead as f64 / payload as f64 * 100.0
        );
        if total_secs > 0.0 {
            println!(
                "payload bitrate:    {:.3} Mbps",
                payload as f64 * 8.0 / total_secs / 1e6
            );
            println!(
                "mean frame rate:    {:.3} fps",
                sample_count as f64 / total_secs
            );
        }
    }

    let sequence_gaps = fragments
        .windows(2)
        .filter(|w| w[1].sequence != w[0].sequence.wrapping_add(1))
        .count();
    let decode_gaps = fragments
        .windows(2)
        .filter(|w| w[1].decode_time != w[0].decode_time + u64::from(w[0].duration))
        .count();
    println!("sequence gaps:      {sequence_gaps}");
    println!("decode-time gaps:   {decode_gaps}");
    let sync_starts = fragments.iter().filter(|f| f.starts_with_sync).count();
    println!("sync-start frags:   {sync_starts} of {}", fragments.len());

    let mut histogram: HashMap<u32, usize> = HashMap::new();
    for duration in fragments.iter().flat_map(|f| f.sample_durations.iter()) {
        *histogram.entry(*duration).or_default() += 1;
    }
    let mut top: Vec<(u32, usize)> = histogram.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let summary: Vec<String> = top
        .iter()
        .take(5)
        .map(|(ticks, count)| {
            format!(
                "{ticks} ticks ({:.2} ms) x{count}",
                f64::from(*ticks) / 90.0
            )
        })
        .collect();
    println!("sample durations:   {}", summary.join(", "));

    match (first.wall, last.wall) {
        (Some(first_wall), Some(last_wall)) => {
            let wall_delta = last_wall.micros_since(first_wall);
            let media_delta =
                MediaTicks::new((last.decode_time - first.decode_time) as i64).as_micros();
            println!(
                "wall clock:         {first_wall} .. {last_wall}; drift vs media time {:+.1} ms",
                (wall_delta - media_delta) as f64 / 1000.0
            );
        }
        _ => println!("wall clock:         no prft boxes"),
    }
}

fn report_sidecar(args: &Args, fragments: &[FragmentInfo]) {
    let path = index_path(&args.file);
    if !path.exists() {
        println!("sidecar index:      none at {}", path.display());
        return;
    }
    match read_index(&path) {
        Ok((header, records)) => {
            let offsets_agree = records.len() == fragments.len()
                && records.iter().zip(fragments).all(|(r, f)| {
                    r.offset == f.offset && r.len == f.len && r.decode_time == f.decode_time
                });
            println!(
                "sidecar index:      {} records (scanned {} fragments); init_len {}; run {}; offsets {}",
                records.len(),
                fragments.len(),
                header.init_len,
                header.run_id,
                if offsets_agree { "agree" } else { "DISAGREE" }
            );
        }
        Err(err) => println!("sidecar index:      unreadable: {err}"),
    }
}

fn print_fragments(fragments: &[FragmentInfo]) {
    println!();
    println!(
        "{:>6} {:>12} {:>9} {:>12} {:>7} {:>5} {:>4}  wall",
        "seq", "offset", "len", "decode", "dur", "n", "sync"
    );
    for f in fragments {
        println!(
            "{:>6} {:>12} {:>9} {:>12} {:>7} {:>5} {:>4}  {}",
            f.sequence,
            f.offset,
            f.len,
            f.decode_time,
            f.duration,
            f.sample_count,
            if f.starts_with_sync { "yes" } else { "no" },
            f.wall.map_or_else(|| "-".to_owned(), |w| w.to_string())
        );
    }
}
