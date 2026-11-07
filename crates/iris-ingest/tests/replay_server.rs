//! Records from the replay server in both read modes and checks that the
//! fragments carry the file's frames.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_core::id::{StreamKey, StreamKind};
use iris_ingest::{
    FragmentSink, IngestOptions, ReadTicker, SinkError, StreamSource, StreamStats, run_stream,
};
use iris_mp4::fragment::Fragment;
use iris_replay::media::Media;
use iris_replay::server::{Options, Server};
use iris_replay::synth::{self, Plan};
use tokio_util::sync::CancellationToken;

struct Collect(Mutex<Vec<Fragment>>);

impl FragmentSink for Collect {
    fn send(&self, _key: &StreamKey, fragment: Fragment) -> Result<(), SinkError> {
        self.0.lock().unwrap().push(fragment);
        Ok(())
    }
}

async fn record(read_interval: Option<Duration>) -> (Arc<Media>, Arc<StreamStats>, Vec<Fragment>) {
    let plan = Plan {
        gops: 4,
        frames_per_gop: 10,
        idr_len: 20_000,
        trail_len: 2_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = [("cam".to_owned(), Arc::clone(&media))]
        .into_iter()
        .collect();
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, Options::default())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());

    let source = StreamSource {
        key: StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        },
        url: format!("rtsp://{addr}/cam").parse().unwrap(),
        username: String::new(),
        password: String::new(),
    };
    let options = IngestOptions {
        read_interval,
        ticker: read_interval.map(ReadTicker::spawn),
        ..IngestOptions::default()
    };
    let sink = Arc::new(Collect(Mutex::new(Vec::new())));
    let stats = Arc::new(StreamStats::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        options,
        Arc::clone(&sink) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while sink.0.lock().unwrap().len() < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fragments within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    task.await.unwrap();
    let fragments = std::mem::take(&mut *sink.0.lock().unwrap());
    (media, stats, fragments)
}

fn check(media: &Media, stats: &StreamStats, fragments: &[Fragment]) {
    assert!(fragments.len() >= 3);
    assert_eq!(stats.reconnects(), 0);
    assert_eq!(stats.dropped_sink(), 0);
    assert!(stats.frames() >= 30, "frames {}", stats.frames());
    assert!(stats.sender_reports() >= 1);
    assert!(
        stats
            .camera_offset_us()
            .is_some_and(|us| us.abs() < 1_000_000)
    );
    for (i, fragment) in fragments.iter().take(2).enumerate() {
        assert_eq!(fragment.samples.len(), 10, "fragment {i}");
        assert!(fragment.starts_with_sync());
        for (j, sample) in fragment.samples.iter().enumerate() {
            let frame = &media.frames[(i * 10 + j) % media.frames.len()];
            assert_eq!(
                &sample.data[..],
                media.sample(frame),
                "fragment {i} sample {j}"
            );
            assert_eq!(sample.duration, 3_000);
        }
    }
}

#[tokio::test]
async fn packet_driven_reads_record_the_replayed_frames() {
    let (media, stats, fragments) = record(None).await;
    check(&media, &stats, &fragments);
}

#[tokio::test]
async fn timer_driven_reads_record_the_replayed_frames() {
    let (media, stats, fragments) = record(Some(Duration::from_millis(20))).await;
    check(&media, &stats, &fragments);
}

#[tokio::test]
async fn shutdown_flushes_every_received_frame_and_clears_connection_state() {
    for interval in [None, Some(Duration::from_millis(20))] {
        for _ in 0..8 {
            let (_, stats, fragments) = record(interval).await;
            let persisted: usize = fragments
                .iter()
                .map(|fragment| fragment.samples.len())
                .sum();
            assert_eq!(
                persisted as u64,
                stats.frames(),
                "shutdown discarded pending frames"
            );
            assert!(!stats.connected(), "shutdown left the stream connected");
        }
    }
}

/// A camera that stops sending: the detached socket's reads return nothing,
/// the inactivity timeout fires, the session tears down and reconnects.
#[tokio::test]
async fn timer_driven_reads_reconnect_after_a_stall() {
    let plan = Plan {
        gops: 4,
        frames_per_gop: 10,
        idr_len: 20_000,
        trail_len: 2_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = [("cam".to_owned(), Arc::clone(&media))]
        .into_iter()
        .collect();
    let options = Options {
        stall_after: Some(Duration::from_millis(1_500)),
        stall_for: Duration::from_secs(30),
        ..Options::default()
    };
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, options)
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());
    let source = StreamSource {
        key: StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        },
        url: format!("rtsp://{addr}/cam").parse().unwrap(),
        username: String::new(),
        password: String::new(),
    };
    let interval = Duration::from_millis(20);
    let options = IngestOptions {
        read_interval: Some(interval),
        ticker: Some(ReadTicker::spawn(interval)),
        inactivity_timeout: Duration::from_secs(1),
        ..IngestOptions::default()
    };
    let sink = Arc::new(Collect(Mutex::new(Vec::new())));
    let stats = Arc::new(StreamStats::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        options,
        Arc::clone(&sink) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while stats.reconnects() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no reconnect after a stall (fragments: {})",
            sink.0.lock().unwrap().len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = sink.0.lock().unwrap().len();
    assert!(before >= 1, "nothing recorded before the stall");
    // The new session delivers again until its own stall.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while sink.0.lock().unwrap().len() <= before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no fragments after the reconnect"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("stream task ends on shutdown")
        .unwrap();
}
