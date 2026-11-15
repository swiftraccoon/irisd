//! Records from the replay server in both read modes and checks that the
//! fragments carry the file's frames.

#![expect(
    clippy::missing_assert_message,
    clippy::unwrap_used,
    reason = "a helper of a test fails as the test does, by panicking"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_core::id::{StreamKey, StreamKind};
use iris_ingest::{
    FragmentSink, IngestOptions, LiveError, LiveFrame, LiveRing, ReadTicker, SinkError, SoundSteps,
    StreamSource, StreamStats, run_stream,
};
use iris_mp4::fragment::{Fragment, Sample};
use iris_replay::media::Media;
use iris_replay::rtp::RtpTimestamp;
use iris_replay::rtsp::MountName;
use iris_replay::server::{AudioStamps, Clocks, LeftOut, Mounts, Options, Server, audio_frame};
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
    record_from(Options::default(), read_interval, 3).await
}

/// Records from a server with `server_options` until `at_least` fragments
/// arrived.
async fn record_from(
    server_options: Options,
    read_interval: Option<Duration>,
    at_least: usize,
) -> (Arc<Media>, Arc<StreamStats>, Vec<Fragment>) {
    let plan = Plan {
        gops: 4,
        frames_per_gop: 10,
        idr_len: 20_000,
        trail_len: 2_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), Arc::clone(&media))]);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, server_options)
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
    while sink.0.lock().unwrap().len() < at_least {
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
    assert!(fragments.len() >= 3, "only {} fragments", fragments.len());
    assert_eq!(stats.reconnects(), 0);
    assert_eq!(stats.dropped_sink(), 0);
    assert!(stats.frames() >= 30, "frames {}", stats.frames());
    assert!(stats.sender_reports() >= 1, "no sender report received");
    assert!(
        stats
            .camera_offset_us()
            .is_some_and(|us| us.abs() < 1_000_000),
        "camera offset {:?}",
        stats.camera_offset_us()
    );
    for (i, fragment) in fragments.iter().take(2).enumerate() {
        assert_eq!(fragment.samples.len(), 10, "fragment {i}");
        assert!(fragment.starts_with_sync());
        for (j, sample) in fragment.samples.iter().enumerate() {
            let frame = &media.frames()[(i * 10 + j) % media.frames().len()];
            assert_eq!(
                Some(&sample.data[..]),
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

/// A camera whose 32-bit RTP timestamp wraps inside the stream, as every
/// camera's does after 13 hours at 90 kHz: the recording runs on through the
/// wrap with every frame at its nominal duration and no snap or reconnect.
#[tokio::test]
async fn an_rtp_timestamp_wrap_inside_the_stream_changes_nothing() {
    // The first frame is one second below the wrap; a fragment lasts a third
    // of a second, so the wrap falls inside the fourth.
    let options = Options {
        base_rtp: Some(RtpTimestamp::new(u32::MAX - 90_000)),
        ..Options::default()
    };
    for interval in [None, Some(Duration::from_millis(20))] {
        let (media, stats, fragments) = record_from(options.clone(), interval, 9).await;
        check(&media, &stats, &fragments);
        assert_eq!((stats.snaps(), stats.holds()), (0, 0));
        assert_eq!(fragments[0].raw_rtp, u32::MAX - 90_000);
        let wrapped = fragments
            .windows(2)
            .filter(|pair| pair[1].raw_rtp < pair[0].raw_rtp)
            .count();
        assert_eq!(wrapped, 1, "the raw timestamp wraps once");
        for (index, fragment) in fragments.iter().enumerate() {
            assert_eq!(fragment.decode_time, index as u64 * 30_000, "{index}");
            assert!(
                fragment
                    .samples
                    .iter()
                    .all(|sample| sample.duration == 3_000)
            );
            assert_eq!(fragment.new_run, index == 0, "{index}");
        }
    }
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
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), Arc::clone(&media))]);
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

/// The longest the task's count of beats stood still over `span`, sampled
/// every 50 ms.
async fn longest_still(stats: &StreamStats, span: Duration) -> Duration {
    let end = tokio::time::Instant::now() + span;
    let (mut beats, mut since) = (stats.beats(), tokio::time::Instant::now());
    let mut longest = Duration::ZERO;
    while tokio::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if stats.beats() != beats {
            (beats, since) = (stats.beats(), tokio::time::Instant::now());
        }
        longest = longest.max(since.elapsed());
    }
    longest
}

/// A camera that plays and then sends nothing is waited on for the
/// inactivity timeout, thirty seconds. Through that wait the task notes
/// that it is alive within every six seconds, in both read modes, with no
/// frame and no reconnection to show for it.
#[tokio::test]
async fn a_task_notes_that_it_is_alive_while_its_camera_sends_nothing() {
    tokio::join!(
        alive_while_silent(None),
        alive_while_silent(Some(Duration::from_millis(20)))
    );
}

/// Records a camera that stalls after half a second and checks the task's
/// beats through twelve seconds of its silence.
async fn alive_while_silent(read_interval: Option<Duration>) {
    let plan = Plan {
        gops: 4,
        frames_per_gop: 10,
        idr_len: 20_000,
        trail_len: 2_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), media)]);
    let options = Options {
        stall_after: Some(Duration::from_millis(500)),
        stall_for: Duration::from_secs(60),
        ..Options::default()
    };
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, options)
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let serving = tokio::spawn(server.run());
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
    let stats = Arc::new(StreamStats::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        options,
        Arc::new(Collect(Mutex::new(Vec::new()))) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    // The frames of the first half second, then the stall.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let frames = stats.frames();
    assert!(frames >= 1, "nothing played before the stall");
    let still = longest_still(&stats, Duration::from_secs(12)).await;
    assert_eq!(
        (stats.frames(), stats.reconnects()),
        (frames, 0),
        "the camera stays connected and silent"
    );
    assert!(
        still <= Duration::from_secs(6),
        "{read_interval:?}: the task stood still for {still:?}"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("stream task ends on shutdown")
        .unwrap();
    serving.abort();
}

/// A camera that refuses the connection is tried again after a pause that
/// doubles, to eight seconds or more by the fourth attempt. Through those
/// pauses the task notes that it is alive within every six seconds.
#[tokio::test]
async fn a_task_notes_that_it_is_alive_while_its_camera_is_down() {
    // A port that nothing listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let source = StreamSource {
        key: StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        },
        url: format!("rtsp://{addr}/cam").parse().unwrap(),
        username: String::new(),
        password: String::new(),
    };
    let stats = Arc::new(StreamStats::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        IngestOptions::default(),
        Arc::new(Collect(Mutex::new(Vec::new()))) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    let still = longest_still(&stats, Duration::from_secs(18)).await;
    assert!(stats.reconnects() >= 4, "{} attempts", stats.reconnects());
    assert_eq!(stats.frames(), 0);
    assert!(
        still <= Duration::from_secs(6),
        "the task stood still for {still:?}"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("stream task ends on shutdown")
        .unwrap();
}

/// The window of the watches of parted clocks in these tests.
const WATCH_WINDOW: Duration = Duration::from_secs(2);

struct CollectByKind(Mutex<Vec<(StreamKind, Fragment)>>);

impl FragmentSink for CollectByKind {
    fn send(&self, key: &StreamKey, fragment: Fragment) -> Result<(), SinkError> {
        self.0.lock().unwrap().push((key.kind, fragment));
        Ok(())
    }
}

/// Records the synthetic camera's video and audio through one session until
/// `enough` says so, and returns what the sink received in order.
async fn record_with_audio(
    server_options: Options,
    enough: impl Fn(&[(StreamKind, Fragment)]) -> bool,
) -> (
    Vec<(StreamKind, Fragment)>,
    Arc<StreamStats>,
    Vec<Arc<LiveFrame>>,
) {
    record_audio(server_options, false, enough).await
}

/// The same, with the steps of the drift rule made on the recording when
/// `audio_steps` is set.
async fn record_audio(
    server_options: Options,
    audio_steps: bool,
    enough: impl Fn(&[(StreamKind, Fragment)]) -> bool,
) -> (
    Vec<(StreamKind, Fragment)>,
    Arc<StreamStats>,
    Vec<Arc<LiveFrame>>,
) {
    let plan = Plan {
        gops: 4,
        frames_per_gop: 10,
        idr_len: 20_000,
        trail_len: 2_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), Arc::clone(&media))]);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, server_options)
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
    let ring = LiveRing::new(256);
    let mut listener = ring.subscribe();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let listening = tokio::spawn({
        let heard = Arc::clone(&heard);
        async move {
            loop {
                match listener.next().await {
                    Ok(frame) => heard.lock().unwrap().push(frame),
                    Err(LiveError::Lagged(_)) => {}
                    Err(LiveError::Closed) => break,
                }
            }
        }
    });
    let options = IngestOptions {
        audio: true,
        audio_live: Some(ring),
        watch_window: WATCH_WINDOW,
        audio_steps,
        ..IngestOptions::default()
    };
    let sink = Arc::new(CollectByKind(Mutex::new(Vec::new())));
    let stats = Arc::new(StreamStats::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        options,
        Arc::clone(&sink) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !enough(&sink.0.lock().unwrap()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not enough recorded within the deadline: {} fragments, {} reconnects",
            sink.0.lock().unwrap().len(),
            stats.reconnects()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    task.await.unwrap();
    // The session held the ring; the listener ends when it is gone.
    tokio::time::timeout(Duration::from_secs(5), listening)
        .await
        .expect("the ring closes with its session")
        .unwrap();
    let collected = std::mem::take(&mut *sink.0.lock().unwrap());
    let heard = std::mem::take(&mut *heard.lock().unwrap());
    (collected, stats, heard)
}

/// What the live ring carried is what was recorded: every live frame is a
/// sync frame of one frame's duration under the audio key, and each one in
/// the span of the recorded fragments has the decode time and the bytes of
/// a recorded sample. Returns how many were matched.
fn check_live_audio(live: &[Arc<LiveFrame>], audio: &[&Fragment]) -> usize {
    let recorded: std::collections::HashMap<u64, &[u8]> = audio
        .iter()
        .flat_map(|fragment| {
            fragment.samples.iter().enumerate().map(|(index, sample)| {
                (
                    fragment.decode_time + index as u64 * 1_440,
                    &sample.data[..],
                )
            })
        })
        .collect();
    let mut matched = 0;
    for frame in live {
        assert_eq!(frame.key.kind, StreamKind::Audio);
        assert!(frame.sample_entry.is_audio());
        assert!(frame.is_sync);
        assert_eq!(frame.duration, 1_440);
        if let Some(sample) = recorded.get(&frame.decode_time) {
            assert_eq!(*sample, &frame.data[..], "at {}", frame.decode_time);
            matched += 1;
        }
    }
    matched
}

fn of_kind(collected: &[(StreamKind, Fragment)], kind: StreamKind) -> Vec<&Fragment> {
    collected
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, fragment)| fragment)
        .collect()
}

/// A lost audio packet breaks the audio's continuity, and the video starts a
/// run with it: both streams have their discontinuity at the same point, the
/// audio run starts within a frame of the video run's start, and no frame
/// is recorded twice.
#[tokio::test]
async fn a_lost_audio_packet_starts_a_run_in_both_streams() {
    let server_options = Options {
        audio_sample_rate: Some(64_000),
        audio_lose_frame: Some(150),
        ..Options::default()
    };
    let runs = |collected: &[(StreamKind, Fragment)], kind| {
        of_kind(collected, kind)
            .iter()
            .filter(|fragment| fragment.new_run)
            .count()
    };
    let (collected, stats, live) = record_with_audio(server_options, |collected| {
        runs(collected, StreamKind::Audio) >= 2 && of_kind(collected, StreamKind::Audio).len() >= 16
    })
    .await;
    assert_eq!(stats.reconnects(), 0);
    assert_eq!(runs(&collected, StreamKind::Main), 2, "video runs");
    assert_eq!(runs(&collected, StreamKind::Audio), 2, "audio runs");

    let audio = of_kind(&collected, StreamKind::Audio);
    let second_run = audio.iter().rposition(|fragment| fragment.new_run).unwrap();
    let restart = audio[second_run];
    assert!(
        restart.decode_time < 1_440,
        "the audio run starts {} ticks into the video run",
        restart.decode_time
    );
    let index_of = |fragment: &Fragment, sample: usize| {
        u64::from_be_bytes(fragment.samples[sample].data[..8].try_into().unwrap())
    };
    let last_before = audio[second_run - 1];
    assert!(index_of(last_before, last_before.samples.len() - 1) < 150);
    assert!(index_of(restart, 0) > 150);
    let indexes: Vec<u64> = audio
        .iter()
        .flat_map(|fragment| (0..fragment.samples.len()).map(|sample| index_of(fragment, sample)))
        .collect();
    assert!(indexes.is_sorted_by(|a, b| a < b), "a frame recorded twice");

    // The live frames stop at the break and resume on the run that follows,
    // once it is placed: none of them is the lost frame's neighbour on the
    // wrong timeline, and each is a frame the recording holds at that time.
    let matched = check_live_audio(&live, &audio);
    assert!(matched > 100, "{matched} live frames matched");
    let live_indexes: Vec<u64> = live
        .iter()
        .map(|frame| u64::from_be_bytes(frame.data[..8].try_into().unwrap()))
        .collect();
    assert!(
        live_indexes.is_sorted_by(|a, b| a < b),
        "a frame sent twice"
    );
    let steps_back = live
        .windows(2)
        .filter(|pair| pair[1].decode_time < pair[0].decode_time)
        .count();
    assert_eq!(
        steps_back, 1,
        "the live timeline restarts once, with the run"
    );
}

/// A camera that offers AAC beside its video: with audio recording on, the
/// session sets the audio stream up too and delivers it under the camera's
/// `audio` key as two-second fragments of the frames as sent, every one a
/// sync sample of one frame's duration, while the video records as before.
#[tokio::test]
async fn the_aac_stream_of_a_session_is_recorded_beside_its_video() {
    // Stamped as cameras stamp audio: some frames early, backwards too.
    let server_options = Options {
        audio_sample_rate: Some(64_000),
        audio_timestamp_wander: true,
        ..Options::default()
    };
    let (collected, stats, live) = record_with_audio(server_options, |collected| {
        of_kind(collected, StreamKind::Audio).len() >= 12
    })
    .await;
    // The live frames are the recorded ones, on the recording's timeline:
    // contiguous, and none before the run was placed, which the close of
    // the second video fragment does.
    let recorded = of_kind(&collected, StreamKind::Audio);
    let video_starts: Vec<u64> = of_kind(&collected, StreamKind::Main)
        .iter()
        .map(|fragment| fragment.decode_time)
        .collect();
    let matched = check_live_audio(&live, &recorded);
    let last_recorded = recorded.last().unwrap();
    let recorded_end = last_recorded.decode_time + last_recorded.samples.len() as u64 * 1_440;
    let in_recording = live
        .iter()
        .filter(|frame| frame.decode_time < recorded_end)
        .count();
    assert!(matched > 150, "{matched} live frames matched");
    assert_eq!(matched, in_recording, "a live frame the recording lacks");
    assert!(
        live[0].decode_time >= video_starts[2],
        "live audio at {} before the run was placed",
        live[0].decode_time
    );
    assert!(
        live.windows(2)
            .all(|pair| pair[1].decode_time == pair[0].decode_time + 1_440),
        "live audio is not contiguous"
    );
    // A live frame's wall time is its place in the video's wall time.
    let first_wall = live[0].wall;
    for frame in &live {
        let media_us = i64::try_from(frame.decode_time - live[0].decode_time).unwrap() * 100 / 9;
        let apart = frame.wall.micros_since(first_wall) - media_us;
        assert!(apart.abs() < 20_000, "wall time {apart} us from media time");
    }
    let (audio, video): (Vec<_>, Vec<_>) = collected
        .into_iter()
        .partition(|(kind, _)| *kind == StreamKind::Audio);
    assert!(video.iter().all(|(kind, _)| *kind == StreamKind::Main));
    assert!(video.len() >= 10, "{} video fragments", video.len());
    assert_eq!(stats.reconnects(), 0);

    check_audio_on_the_video_timeline(&audio, &video);
}

/// The audio lies on the video's timeline: every audio fragment starts within
/// one audio frame after a video fragment does, the fragments are contiguous,
/// and the frames are the ones sent, in order, none twice.
fn check_audio_on_the_video_timeline(
    audio: &[(StreamKind, Fragment)],
    video: &[(StreamKind, Fragment)],
) {
    let video_starts: Vec<u64> = video.iter().map(|(_, f)| f.decode_time).collect();
    let first_frame = u64::from_be_bytes(audio[0].1.samples[0].data[..8].try_into().unwrap());
    let mut next_frame = first_frame;
    for (index, (_, fragment)) in audio.iter().enumerate() {
        assert!(fragment.sample_entry.is_audio());
        assert_eq!(fragment.sample_entry.codecs, "mp4a.40.2");
        assert_eq!(fragment.new_run, index == 0, "{index}");
        assert!(
            video_starts
                .iter()
                .any(|start| (*start..start + 1_440).contains(&fragment.decode_time)),
            "audio fragment {index} at {} starts with no video fragment: {video_starts:?}",
            fragment.decode_time
        );
        if index > 0 {
            let before = &audio[index - 1].1;
            assert_eq!(
                fragment.decode_time,
                before.decode_time + before.duration(),
                "{index}"
            );
        }
        for sample in &fragment.samples {
            assert!(sample.is_sync);
            assert_eq!(sample.duration, 1_440);
            assert_eq!(
                &sample.data[..],
                audio_frame(next_frame),
                "frame {next_frame}"
            );
            next_frame += 1;
        }
    }
    // The run starts with the video: at most the frames of the first half
    // second, sent before the first video frame was placed, are left out.
    assert!(first_frame < 32, "audio starts at frame {first_frame}");
    assert!(next_frame - first_frame >= 200, "{next_frame} audio frames");
}

/// What the watches of parted clocks read last, in milliseconds, and what
/// the drift rule said.
#[derive(Debug)]
struct Watched {
    /// How far the video's timestamps moved against the recorder's clock.
    clock: f64,
    /// How far the audio's timestamps moved against its frame count.
    stamps: f64,
    /// How late the sender reports find the recorded audio against the
    /// video, and how far that moved.
    by_reports: Option<(f64, f64)>,
    /// How far the audio moved against the video by when frames arrived.
    by_arrival: f64,
    /// Frames the rule said it would leave out, and record twice.
    steps_said: SoundSteps,
}

/// Records the synthetic camera with `clocks` for five windows of the
/// watches of parted clocks and returns what they read last.
async fn watched(clocks: Clocks) -> Watched {
    watched_for(clocks, 5).await
}

/// The same over `windows` windows.
async fn watched_for(clocks: Clocks, windows: u64) -> Watched {
    watched_run(clocks, windows, None).await
}

/// The same of a camera that leaves `leave_out` of its sound out.
async fn watched_run(clocks: Clocks, windows: u64, leave_out: Option<LeftOut>) -> Watched {
    let server_options = Options {
        audio_sample_rate: Some(64_000),
        audio_leave_out: leave_out,
        clocks,
        // Reports this often place a frame to within a tenth of a
        // millisecond at the faults injected here.
        report_interval: Duration::from_millis(20),
        ..Options::default()
    };
    let span = |collected: &[(StreamKind, Fragment)]| {
        let audio = of_kind(collected, StreamKind::Audio);
        match (audio.first(), audio.last()) {
            (Some(first), Some(last)) => last.decode_time - first.decode_time,
            _ => 0,
        }
    };
    let enough = windows * WATCH_WINDOW.as_secs() * 90_000 + 45_000;
    let (_, stats, _) =
        record_with_audio(server_options, |collected| span(collected) >= enough).await;
    let millis = |micros: i64| micros as f64 / 1_000.0;
    let watched = Watched {
        clock: millis(
            stats
                .clock_moved_us()
                .expect("a window of the video's clock"),
        ),
        stamps: millis(
            stats
                .audio_stamps_moved_us()
                .expect("a window of the audio's stamps"),
        ),
        by_reports: stats
            .audio_sync_us()
            .zip(stats.audio_sync_moved_us())
            .map(|(late, moved)| (millis(late), millis(moved))),
        by_arrival: millis(
            stats
                .audio_arrival_moved_us()
                .expect("a window of the audio's arrival"),
        ),
        steps_said: stats.sound_steps_said(),
    };
    println!("{clocks:?}: {watched:?}");
    watched
}

/// Asserts that `value` is `expected` to within `tolerance`.
#[track_caller]
fn assert_near(value: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (value - expected).abs() <= tolerance,
        "{what}: {value:.1} ms, expected {expected:.1} within {tolerance:.1}"
    );
}

/// With every clock exact nothing moves, the sender reports put the
/// recorded audio where the camera has it, and the rule would make no step.
#[tokio::test]
async fn exact_clocks_show_no_drift() {
    let clocks = Clocks {
        audio_reports: true,
        ..Clocks::default()
    };
    let watched = watched(clocks).await;
    let (late, moved) = watched.by_reports.expect("reports for both streams");
    assert_near(
        watched.clock,
        0.0,
        3.0,
        "the video's clock against the recorder's",
    );
    assert_near(
        watched.stamps,
        0.0,
        0.5,
        "the audio's stamps against its count",
    );
    assert_near(
        moved,
        0.0,
        2.0,
        "the recorded audio against the video, since the first window",
    );
    assert_near(late, 0.0, 10.0, "the recorded audio against the video");
    assert_near(
        watched.by_arrival,
        0.0,
        3.0,
        "the recorded audio against the video by arrival",
    );
    assert_eq!(watched.steps_said, SoundSteps::default());
}

/// The same camera with its sender reports stamped as cameras stamp them,
/// behind the clock by up to a frame and a few ahead of it: the reports
/// still put the recorded audio where the camera has it.
#[tokio::test]
async fn late_sender_reports_are_read_through() {
    let clocks = Clocks {
        audio_reports: true,
        late_reports: true,
        ..Clocks::default()
    };
    let watched = watched(clocks).await;
    let (late, moved) = watched.by_reports.expect("reports for both streams");
    assert_near(
        moved,
        0.0,
        3.0,
        "the recorded audio against the video, since the first window",
    );
    assert_near(late, 0.0, 10.0, "the recorded audio against the video");
    assert_eq!(watched.steps_said, SoundSteps::default());
}

/// A sample clock 5,000 ppm fast under a camera that stamps its audio from
/// its media clock: each two-second window the count runs 10 ms further
/// ahead of the stamps, and the recorded audio falls that much later
/// against the video, by the sender reports and by arrival alike. Four
/// windows separate the last window of the stamps from the first, and three
/// and a half those of the two readings, whose windows are counted in
/// fragments. Past three quarters of a 16 ms frame for two windows, the
/// rule would leave frames out.
#[tokio::test]
async fn a_fast_sample_clock_shows_in_camera_clock_stamps_and_in_both_readings() {
    let clocks = Clocks {
        audio_fast_ppm: 5_000,
        audio_stamps: AudioStamps::CameraClock,
        audio_reports: true,
        late_reports: true,
        ..Clocks::default()
    };
    let watched = watched(clocks).await;
    let (_, moved) = watched.by_reports.expect("reports for both streams");
    assert_near(
        watched.clock,
        0.0,
        3.0,
        "the video's clock against the recorder's",
    );
    assert_near(
        watched.stamps,
        -40.0,
        2.0,
        "the audio's stamps against its count",
    );
    assert_near(moved, 35.0, 4.0, "the recorded audio against the video");
    assert_near(
        watched.by_arrival,
        35.0,
        4.0,
        "the recorded audio against the video by arrival",
    );
    let SoundSteps {
        left_out,
        recorded_twice,
    } = watched.steps_said;
    assert!((1..=3).contains(&left_out), "{left_out} frames left out");
    assert_eq!(recorded_twice, 0);
}

/// The same fast sample clock under a camera that stamps its audio by the
/// sample count, as RFC 3550 §5.1 asks: the stamps are the count and show
/// nothing, and the two readings of the audio against the video show it.
#[tokio::test]
async fn a_fast_sample_clock_under_sample_clock_stamps_shows_in_both_readings() {
    let clocks = Clocks {
        audio_fast_ppm: 5_000,
        audio_stamps: AudioStamps::SampleClock,
        audio_reports: true,
        ..Clocks::default()
    };
    let watched = watched(clocks).await;
    let (_, moved) = watched.by_reports.expect("reports for both streams");
    assert_near(
        watched.stamps,
        0.0,
        0.5,
        "the audio's stamps against its count",
    );
    assert_near(moved, 35.0, 4.0, "the recorded audio against the video");
    assert_near(
        watched.by_arrival,
        35.0,
        4.0,
        "the recorded audio against the video by arrival",
    );
}

/// Without sender reports for the audio nothing says where the camera has
/// it against the video; its arrival and its timestamps still show the
/// drift, 10 ms a window. The rule judges the median of a run's last
/// three windows against the median of its first three, and would leave
/// out three to five frames of the 70 ms that the last three windows stand
/// from the first three, by where the run's windows fall. Nine windows separate the last window of the
/// stamps from the first, and about as many those of the reading by
/// arrival.
#[tokio::test]
async fn without_audio_sender_reports_the_drift_shows_by_arrival_and_stamps() {
    let clocks = Clocks {
        audio_fast_ppm: 5_000,
        audio_stamps: AudioStamps::CameraClock,
        ..Clocks::default()
    };
    let watched = watched_for(clocks, 10).await;
    assert_eq!(watched.by_reports, None);
    assert_near(
        watched.stamps,
        -90.0,
        3.0,
        "the audio's stamps against its count",
    );
    assert_near(
        watched.by_arrival,
        90.0,
        8.0,
        "the recorded audio against the video by arrival",
    );
    assert!(
        (3..=5).contains(&watched.steps_said.left_out),
        "{:?}",
        watched.steps_said
    );
    assert_eq!(watched.steps_said.recorded_twice, 0);
}

/// A camera with exact clocks that leaves seven frames of its sound out,
/// 112 ms, in the seventh window: the frames after them are stamped and
/// arrive in their own time, so the count that places them is 112 ms
/// behind and the recording has the sound that much early. The audio's
/// stamps move against its count and its arrival against the video's by as
/// much, and the rule, which reads a run without reports by both, would
/// record three frames twice within the ten windows.
#[tokio::test]
async fn sound_a_camera_leaves_out_shows_by_arrival_and_stamps() {
    let clocks = Clocks {
        audio_stamps: AudioStamps::CameraClock,
        ..Clocks::default()
    };
    // Frame 800 starts 12.8 s after the session does.
    let leave_out = LeftOut { at: 800, frames: 7 };
    let watched = watched_run(clocks, 10, Some(leave_out)).await;
    assert_eq!(watched.by_reports, None);
    assert_near(
        watched.stamps,
        112.0,
        2.0,
        "the audio's stamps against its count",
    );
    assert_near(
        watched.by_arrival,
        -112.0,
        4.0,
        "the recorded audio against the video by arrival",
    );
    assert_eq!(
        watched.steps_said,
        SoundSteps {
            left_out: 0,
            recorded_twice: 3
        }
    );
}

/// The same camera with exact clocks: the rule would make no step over
/// the same ten windows.
#[tokio::test]
async fn without_audio_sender_reports_exact_clocks_show_no_drift() {
    let watched = watched_for(Clocks::default(), 10).await;
    assert_eq!(watched.by_reports, None);
    assert_near(
        watched.by_arrival,
        0.0,
        3.0,
        "the recorded audio against the video by arrival",
    );
    assert_eq!(watched.steps_said, SoundSteps::default());
}

/// A camera whose media clock runs 5,000 ppm fast delivers each two-second
/// window 10 ms sooner against its timestamps; its audio follows the same
/// clock, so nothing moves between the two.
#[tokio::test]
async fn a_fast_camera_clock_shows_against_the_recorders_clock_alone() {
    let clocks = Clocks {
        camera_fast_ppm: 5_000,
        audio_stamps: AudioStamps::CameraClock,
        audio_reports: true,
        ..Clocks::default()
    };
    let watched = watched(clocks).await;
    let (_, moved) = watched.by_reports.expect("reports for both streams");
    assert_near(
        watched.clock,
        -41.0,
        4.0,
        "the video's clock against the recorder's",
    );
    assert_near(
        watched.stamps,
        0.0,
        0.5,
        "the audio's stamps against its count",
    );
    assert_near(moved, 0.0, 2.0, "the recorded audio against the video");
    assert_near(
        watched.by_arrival,
        0.0,
        3.0,
        "the recorded audio against the video by arrival",
    );
    assert_eq!(watched.steps_said, SoundSteps::default());
}

/// What a recording made with the steps of the drift rule holds.
struct Stepped {
    /// How late the sender reports find the recorded audio in the last
    /// window, in milliseconds.
    late: f64,
    said: SoundSteps,
    made: SoundSteps,
    /// The camera's frames that the recording lacks between two it has.
    absent: u64,
    /// The frames the recording has a second time.
    again: u64,
    /// The fragments marked as holding frames left out, and frames twice.
    marked: (usize, usize),
}

/// Records the synthetic camera, its sample clock `ppm` fast against its
/// media clock, for twenty windows with the steps of the drift rule made,
/// and checks what every such recording holds: decode times that stay a
/// count through the steps, a mark on each fragment that holds one and on
/// no other, and a live ring that carried what was recorded.
async fn stepped(ppm: i32) -> Stepped {
    let server_options = Options {
        audio_sample_rate: Some(64_000),
        clocks: Clocks {
            audio_fast_ppm: ppm,
            audio_stamps: AudioStamps::CameraClock,
            audio_reports: true,
            late_reports: true,
            ..Clocks::default()
        },
        report_interval: Duration::from_millis(20),
        ..Options::default()
    };
    let enough = 20 * WATCH_WINDOW.as_secs() * 90_000 + 45_000;
    let (collected, stats, live) = record_audio(server_options, true, |collected| {
        let audio = of_kind(collected, StreamKind::Audio);
        match (audio.first(), audio.last()) {
            (Some(first), Some(last)) => last.decode_time - first.decode_time >= enough,
            _ => false,
        }
    })
    .await;
    let audio = of_kind(&collected, StreamKind::Audio);
    let number = |sample: &Sample| u64::from_be_bytes(sample.data[..8].try_into().unwrap());
    let (mut absent, mut again) = (0, 0);
    let mut marked = (0, 0);
    let mut last: Option<u64> = None;
    for (index, fragment) in audio.iter().enumerate() {
        if index > 0 {
            let before = audio[index - 1];
            assert_eq!(
                fragment.decode_time,
                before.decode_time + before.duration(),
                "audio fragment {index} does not follow the one before it"
            );
        }
        let (mut left_out, mut twice) = (false, false);
        for sample in &fragment.samples {
            assert_eq!(sample.duration, 1_440);
            let frame = number(sample);
            assert_eq!(&sample.data[..], audio_frame(frame), "frame {frame}");
            match last {
                Some(last) if frame == last => {
                    again += 1;
                    twice = true;
                }
                Some(last) => {
                    assert!(frame > last, "frame {frame} after frame {last}");
                    absent += frame - last - 1;
                    left_out |= frame > last + 1;
                }
                None => {}
            }
            last = Some(frame);
        }
        assert_eq!(
            (fragment.steps.left_out, fragment.steps.twice),
            (left_out, twice),
            "the marks of audio fragment {index}"
        );
        marked.0 += usize::from(left_out);
        marked.1 += usize::from(twice);
    }
    let matched = check_live_audio(&live, &audio);
    let recorded_end = audio
        .last()
        .map_or(0, |last| last.decode_time + last.duration());
    let in_recording = live
        .iter()
        .filter(|frame| frame.decode_time < recorded_end)
        .count();
    assert!(matched > 1_500, "{matched} live frames matched");
    assert_eq!(matched, in_recording, "a live frame the recording lacks");
    assert!(
        live.windows(2)
            .all(|pair| pair[1].decode_time == pair[0].decode_time + 1_440),
        "live audio is not contiguous"
    );
    assert_eq!(stats.reconnects(), 0);
    let stepped = Stepped {
        late: stats.audio_sync_us().expect("a window read by reports") as f64 / 1_000.0,
        said: stats.sound_steps_said(),
        made: stats.sound_steps_made(),
        absent,
        again,
        marked,
    };
    println!(
        "{ppm} ppm: late {:.1} ms, said {:?}, made {:?}, absent {absent}, again {again}, \
         marked {marked:?}",
        stepped.late, stepped.said, stepped.made
    );
    stepped
}

/// With the steps made, a sample clock 2,000 ppm fast, which moves the
/// sound 4 ms later a window and 80 ms over the twenty, is held: a frame
/// is left out each time the reading has stood past 12 ms for two windows,
/// which it does at 20 ms, so the last window reads within 28 ms of zero
/// after three frames or more. With the frames left out put back it reads
/// 60 ms or more. A rule that took its steps off a place that holds them
/// would step past 12, 28, and 44 ms and end past the bound. The frames the
/// recording lacks are the ones the steps left out.
#[tokio::test]
async fn steps_hold_the_sound_of_a_fast_sample_clock_to_its_picture() {
    let held = stepped(2_000).await;
    assert!(held.late.abs() <= 28.0, "{:.1} ms late", held.late);
    let unheld = held.late + 16.0 * held.made.left_out as f64;
    assert!(unheld >= 60.0, "{unheld:.1} ms late without the steps");
    assert!((3..=5).contains(&held.made.left_out), "{:?}", held.made);
    assert_eq!(held.made.recorded_twice, 0);
    assert_eq!(held.said.recorded_twice, 0);
    // A step said as the recording ends is not made.
    assert!(
        held.said.left_out >= held.made.left_out && held.said.left_out <= held.made.left_out + 1,
        "said {:?}, made {:?}",
        held.said,
        held.made
    );
    assert_eq!((held.absent, held.again), (held.made.left_out, 0));
    assert_eq!(held.marked.1, 0);
    assert!(
        (3..=5).contains(&held.marked.0),
        "{} fragments marked",
        held.marked.0
    );
}

/// The same of a sample clock 2,000 ppm slow: the sound moves earlier, and
/// a frame is recorded twice each time.
#[tokio::test]
async fn steps_hold_the_sound_of_a_slow_sample_clock_to_its_picture() {
    let held = stepped(-2_000).await;
    assert!(held.late.abs() <= 28.0, "{:.1} ms late", held.late);
    let unheld = held.late - 16.0 * held.made.recorded_twice as f64;
    assert!(unheld <= -60.0, "{unheld:.1} ms late without the steps");
    assert!(
        held.said.recorded_twice >= held.made.recorded_twice
            && held.said.recorded_twice <= held.made.recorded_twice + 1,
        "said {:?}, made {:?}",
        held.said,
        held.made
    );
    assert!(
        (3..=5).contains(&held.made.recorded_twice),
        "{:?}",
        held.made
    );
    assert_eq!(held.made.left_out, 0);
    assert_eq!(held.said.left_out, 0);
    assert_eq!((held.absent, held.again), (0, held.made.recorded_twice));
    assert_eq!(held.marked.0, 0);
    assert!(
        (3..=5).contains(&held.marked.1),
        "{} fragments marked",
        held.marked.1
    );
}

/// Audio at a sampling rate whose frames are not whole 90 kHz ticks is not
/// recorded, and the session records its video as if no audio were sent:
/// audio never ends a session.
#[tokio::test]
async fn audio_whose_frames_are_not_whole_ticks_is_left_unrecorded() {
    let server_options = Options {
        audio_sample_rate: Some(44_100),
        ..Options::default()
    };
    let (collected, stats, live) = record_with_audio(server_options, |collected| {
        of_kind(collected, StreamKind::Main).len() >= 6
    })
    .await;
    assert!(of_kind(&collected, StreamKind::Audio).is_empty());
    assert!(live.is_empty(), "{} live audio frames", live.len());
    assert_eq!(stats.reconnects(), 0);
    let video = of_kind(&collected, StreamKind::Main);
    assert!(
        video
            .windows(2)
            .all(|pair| pair[1].decode_time == pair[0].decode_time + pair[0].duration()),
        "the video is not continuous"
    );
}
