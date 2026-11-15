//! Sound in the focused live view: the timing that plays a camera's audio
//! feed and its picture against one clock (`docs/research/13-audio.md`
//! §10.4).
//!
//! Audio frames reach the viewer in bursts, so sound played as it arrives
//! stutters, and a picture shown the instant it is decoded would lead its
//! sound by the delay that smooths it. Both feeds are therefore held the
//! same [`DELAY`]: each keeps the least of arrival time less media time over
//! a window, and a frame plays at its media time plus that least plus the
//! delay. The mapping is a running-time offset on the pad after each feed's
//! demuxer. While a feed's first frames are still held it follows the
//! estimate, which one frame cannot give; after that it moves only when an
//! estimate has left what is applied by [`REANCHOR`], and then for both
//! feeds together.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use tracing::debug;

use crate::fragment;

/// How long after its least-delay arrival a frame plays: more than the
/// longest step between audio frames seen from a camera.
pub const DELAY: Duration = Duration::from_millis(500);

/// How far an estimate moves from the applied mapping before the mapping
/// follows it.
const REANCHOR: Duration = Duration::from_millis(80);

/// The seconds of arrivals an estimate looks back over.
const WINDOW_SECONDS: i64 = 10;

/// Nanoseconds in a second.
const SECOND: i64 = 1_000_000_000;

/// Whether a camera's audio is discarded instead of played on the default
/// output: set by `--mute`, and always in tests.
static SILENT: AtomicBool = AtomicBool::new(cfg!(test));

/// Makes every view discard audio from here on.
pub fn mute() {
    SILENT.store(true, Ordering::Relaxed);
}

/// Whether audio is discarded ([`mute`]).
pub fn silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// One of the two feeds of a camera that play together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Medium {
    /// The video feed.
    Picture = 0,
    /// The audio feed.
    Sound = 1,
}

/// The least of arrival time less media time over the last
/// [`WINDOW_SECONDS`], kept as one least for each second of arrivals.
#[derive(Debug, Default)]
struct LeastDelay {
    /// The second of running time and the least delay seen in it.
    seconds: VecDeque<(i64, i64)>,
}

impl LeastDelay {
    /// Notes an arrival at `running` nanoseconds with `delay` nanoseconds
    /// and returns the least delay of the window that ends there.
    fn note(&mut self, running: i64, delay: i64) -> i64 {
        let second = running.div_euclid(SECOND);
        match self.seconds.back_mut() {
            Some((at, least)) if *at == second => *least = (*least).min(delay),
            _ => self.seconds.push_back((second, delay)),
        }
        while self
            .seconds
            .front()
            .is_some_and(|(old, _)| *old <= second - WINDOW_SECONDS)
        {
            self.seconds.pop_front();
        }
        self.seconds
            .iter()
            .map(|(_, least)| *least)
            .min()
            .unwrap_or(delay)
    }
}

/// One feed's place on the clock.
#[derive(Debug, Default)]
struct Track {
    /// Media time of the byte stream's first fragment, where the demuxer
    /// starts its segment, and the running time at which it arrived.
    first: Option<(i64, i64)>,
    delays: LeastDelay,
    /// The offset the estimate asks for.
    wanted: Option<i64>,
    /// The offset in force.
    applied: Option<i64>,
}

/// The mapping of both feeds, without the pipeline: what [`Pairing`] decides.
#[derive(Debug, Default)]
struct Anchors {
    tracks: [Track; 2],
}

impl Anchors {
    /// A feed starts a byte stream over: its demuxer starts a segment at
    /// the next fragment, and its estimate starts with it.
    fn restart(&mut self, feed: Medium) {
        self.tracks[feed as usize] = Track::default();
    }

    /// Notes a fragment of `feed` with the media time `media` arriving at
    /// the running time `running`, both in nanoseconds. Returns the offset
    /// of each feed to put in force when the mapping moves: with every
    /// change of the estimate while the feed's first frames are held (the
    /// first [`DELAY`] of it, when one frame's delay is all there is and
    /// nothing has played), and after that when the feed's estimate has
    /// left what is applied by [`REANCHOR`].
    fn note(&mut self, feed: Medium, media: i64, running: i64) -> Option<[Option<i64>; 2]> {
        let track = &mut self.tracks[feed as usize];
        let (first, started) = *track.first.get_or_insert((media, running));
        let least = track.delays.note(running, running.saturating_sub(media));
        let settling = running.saturating_sub(started) < nanos(DELAY);
        let tolerance = if settling { 0 } else { nanos(REANCHOR) };
        // A frame's running time after the demuxer is its media time less
        // the first frame's; it is to play at media time plus the least
        // delay plus the delay.
        let wanted = first.saturating_add(least).saturating_add(nanos(DELAY));
        track.wanted = Some(wanted);
        let moved = track
            .applied
            .is_none_or(|applied| wanted.abs_diff(applied) > tolerance.unsigned_abs());
        if !moved {
            return None;
        }
        for track in &mut self.tracks {
            track.applied = track.wanted;
        }
        Some([self.tracks[0].applied, self.tracks[1].applied])
    }
}

fn nanos(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

/// The picture and the sound of one camera, played against the clock of
/// the pipeline their branches are in.
pub struct Pairing {
    /// What tells the running time: the pipeline.
    clock: gst::Element,
    state: Mutex<State>,
}

impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    anchors: Anchors,
    /// The pad after each feed's demuxer, where its offset is set.
    pads: [Option<gst::Pad>; 2],
    /// Whether the mapping is in force: from the sound's first fragment.
    engaged: bool,
    /// Called once, when the mapping comes into force.
    on_engage: Option<Box<dyn FnOnce() + Send>>,
}

impl Pairing {
    /// A pairing on the clock of `pipeline`. `on_engage` runs once, when
    /// the sound's first fragment arrives: until then the picture is not
    /// held, and a camera without sound never holds it.
    pub fn new(pipeline: &gst::Pipeline, on_engage: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Arc::new(Self {
            clock: pipeline.clone().upcast(),
            state: Mutex::new(State {
                on_engage: Some(Box::new(on_engage)),
                ..State::default()
            }),
        })
    }

    /// The pacer of one feed, whose frames pass `pad` after its demuxer.
    pub fn pacer(self: &Arc<Self>, feed: Medium, pad: gst::Pad) -> Pacer {
        self.state.lock().expect("pairing lock").pads[feed as usize] = Some(pad);
        Pacer {
            pairing: Arc::clone(self),
            feed,
        }
    }

    fn restart(&self, feed: Medium) {
        self.state
            .lock()
            .expect("pairing lock")
            .anchors
            .restart(feed);
    }

    fn fragment(&self, feed: Medium, payload: &[u8]) {
        let Some(media) = fragment::media_time(payload) else {
            return;
        };
        let Some(running) = self
            .clock
            .current_running_time()
            .and_then(|time| i64::try_from(time.nseconds()).ok())
        else {
            return;
        };
        let mut state = self.state.lock().expect("pairing lock");
        let moved = state.anchors.note(feed, media, running);
        let engaging = feed == Medium::Sound && !state.engaged;
        state.engaged |= engaging;
        if !state.engaged {
            return;
        }
        // Engaging puts the picture's mapping in force with the sound's.
        let offsets = if engaging {
            [
                state.anchors.tracks[0].applied,
                state.anchors.tracks[1].applied,
            ]
        } else if let Some(offsets) = moved {
            offsets
        } else {
            return;
        };
        for (pad, offset) in state.pads.iter().zip(offsets) {
            if let (Some(pad), Some(offset)) = (pad, offset) {
                pad.set_offset(offset);
            }
        }
        // The picture is held from here on, with its mapping in place.
        if let Some(on_engage) = state.on_engage.take() {
            on_engage();
        }
        debug!(
            picture_ms = offsets[0].map(|offset| offset / 1_000_000),
            sound_ms = offsets[1].map(|offset| offset / 1_000_000),
            "sound and picture placed on the clock"
        );
    }
}

/// One feed's hold on its [`Pairing`]: told of every payload before it is
/// pushed into the branch.
#[derive(Clone, Debug)]
pub struct Pacer {
    pairing: Arc<Pairing>,
    feed: Medium,
}

impl Pacer {
    /// A fresh byte stream follows (a reconnection).
    pub fn restart(&self) {
        self.pairing.restart(self.feed);
    }

    /// Notes a live payload that is about to be pushed; an init segment
    /// changes nothing.
    pub fn payload(&self, payload: &[u8]) {
        self.pairing.fragment(self.feed, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// No test plays a camera's audio on the machine it runs on.
    #[test]
    fn tests_discard_audio() {
        assert!(silent());
        mute();
        assert!(silent());
    }

    #[test]
    fn the_least_delay_is_the_least_of_the_last_ten_seconds() {
        let mut least = LeastDelay::default();
        assert_eq!(least.note(0, 50), 50);
        assert_eq!(least.note(SECOND / 2, 70), 50);
        assert_eq!(least.note(SECOND, 30), 30);
        assert_eq!(least.note(4 * SECOND, 90), 30);
        // Ten seconds after the second that held 30, it has left the window.
        assert_eq!(least.note(10 * SECOND + SECOND / 2, 80), 30);
        assert_eq!(least.note(11 * SECOND, 85), 80);
        assert_eq!(least.note(30 * SECOND, 5), 5);
        assert_eq!(least.seconds.len(), 1);
    }

    /// The `prft` box a live fragment starts with, for the media time
    /// `ticks`.
    fn prft(ticks: u64) -> Vec<u8> {
        let mut fragment = Vec::new();
        fragment.extend_from_slice(&32_u32.to_be_bytes());
        fragment.extend_from_slice(b"prft");
        fragment.extend_from_slice(&[1, 0, 0, 0x18]);
        fragment.extend_from_slice(&1_u32.to_be_bytes());
        fragment.extend_from_slice(&0_u64.to_be_bytes());
        fragment.extend_from_slice(&ticks.to_be_bytes());
        fragment
    }

    /// A pairing sets nothing until the sound's first fragment arrives:
    /// then it sets both pads' offsets and says so, once. An init segment
    /// changes nothing, and a feed that starts over is placed by its next
    /// fragment. The offsets put a first fragment half a second after its
    /// arrival, whatever its media time.
    #[test]
    fn a_pairing_sets_the_pads_from_the_sounds_first_fragment_on() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (_, state, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        assert_eq!(state, gst::State::Playing);
        let engagements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pairing = Pairing::new(&pipeline, {
            let engagements = Arc::clone(&engagements);
            move || {
                engagements.fetch_add(1, Ordering::AcqRel);
            }
        });
        let pad = || gst::Pad::builder(gst::PadDirection::Src).build();
        let (picture_pad, sound_pad) = (pad(), pad());
        let picture = pairing.pacer(Medium::Picture, picture_pad.clone());
        let sound = pairing.pacer(Medium::Sound, sound_pad.clone());
        let running =
            || i64::try_from(pipeline.current_running_time().unwrap().nseconds()).unwrap();
        // The demuxer counts a feed's running time from its first fragment,
        // so the offset of a feed whose first fragment just arrived is the
        // running time of the arrival plus the delay.
        let placed_now = |pad: &gst::Pad, before: i64| {
            let offset = pad.offset();
            let after = running();
            assert!(
                (before + nanos(DELAY)..=after + nanos(DELAY)).contains(&offset),
                "offset {offset} outside {before}..={after} plus the delay"
            );
        };

        // The picture alone: tracked, with nothing set.
        picture.payload(&prft(90_000_000));
        picture.payload(&prft(90_003_000));
        assert_eq!((picture_pad.offset(), sound_pad.offset()), (0, 0));
        assert_eq!(engagements.load(Ordering::Acquire), 0);
        // An init segment of the sound is not a fragment.
        sound.payload(b"\0\0\0\x1cftypiso6\0\0\0\0iso6cmfcmp41");
        assert_eq!(engagements.load(Ordering::Acquire), 0);
        assert_eq!(sound_pad.offset(), 0);

        // The sound's first fragment, on a timeline of its own.
        let before = running();
        sound.payload(&prft(450_000_000));
        assert_eq!(engagements.load(Ordering::Acquire), 1);
        placed_now(&sound_pad, before);
        // The picture's offset is in force too: its first fragment arrived
        // a moment earlier, and the second 33 ms of media after it.
        let picture_offset = picture_pad.offset();
        assert!(picture_offset > 0 && picture_offset <= sound_pad.offset());
        assert!(sound_pad.offset() - picture_offset < 200 * MS);

        // More fragments move nothing and say nothing.
        let settled = sound_pad.offset();
        std::thread::sleep(Duration::from_millis(16));
        sound.payload(&prft(450_001_440));
        assert!(
            (sound_pad.offset() - settled).abs() <= MS,
            "the estimate held"
        );
        assert_eq!(engagements.load(Ordering::Acquire), 1);

        // The sound starts over on another run: placed by its next fragment.
        std::thread::sleep(Duration::from_millis(30));
        sound.restart();
        let before = running();
        sound.payload(&prft(9_000));
        placed_now(&sound_pad, before);
        assert!(sound_pad.offset() > settled + 25 * MS);
        assert_eq!(engagements.load(Ordering::Acquire), 1);
        // The picture starts over too.
        std::thread::sleep(Duration::from_millis(30));
        picture.restart();
        let before = running();
        picture.payload(&prft(18_000));
        placed_now(&picture_pad, before);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// A feed's frames play at media time plus the least delay plus the
    /// delay: the offset is the first media time plus those two, since the
    /// demuxer counts running time from the first frame.
    #[test]
    fn a_feed_is_placed_by_its_least_delay() {
        let mut anchors = Anchors::default();
        // Media time 100 s arrives at running time 7 s, 40 ms late.
        let media = 100 * SECOND;
        let moved = anchors.note(Medium::Picture, media, 7 * SECOND + 40 * MS);
        let first = media + (7 * SECOND + 40 * MS - media) + 500 * MS;
        assert_eq!(moved, Some([Some(first), None]));
        // The next frame arrives on time. The first frames are still held,
        // so the mapping follows the estimate, 40 ms earlier.
        assert_eq!(
            anchors.note(Medium::Picture, media + 33 * MS, 7 * SECOND + 33 * MS),
            Some([Some(first - 40 * MS), None])
        );
        // Half a second after the feed's first frame, to the nanosecond,
        // nothing is held any more: a frame 30 ms earlier still is inside
        // the threshold and moves nothing.
        assert_eq!(
            anchors.note(Medium::Picture, media + 570 * MS, 7 * SECOND + 540 * MS),
            None
        );
        // The sound's first fragment places the sound; the picture takes
        // the estimate it had reached.
        let sound_media = 250 * SECOND;
        let moved = anchors.note(Medium::Sound, sound_media, 8 * SECOND + 60 * MS);
        assert_eq!(
            moved,
            Some([
                Some(first - 70 * MS),
                Some(sound_media + (8 * SECOND + 60 * MS - sound_media) + 500 * MS),
            ])
        );
        // The frames of the sound's first group came together; the last of
        // them is the least delayed, and the sound follows it at once.
        let moved = anchors.note(Medium::Sound, sound_media + 64 * MS, 8 * SECOND + 60 * MS);
        assert_eq!(
            moved.map(|offsets| offsets[1]),
            Some(Some(
                sound_media + (8 * SECOND - 4 * MS - sound_media) + 500 * MS
            ))
        );
    }

    /// A burst of earlier frames at the start puts the first estimate late; the
    /// first live frame moves it, once. Late frames after that change
    /// nothing, and a slow drift moves the mapping when it has grown past
    /// the threshold.
    #[test]
    fn the_mapping_moves_only_past_the_threshold() {
        let mut anchors = Anchors::default();
        let media = 50 * SECOND;
        // The connection's burst: a frame captured two seconds before.
        assert!(anchors.note(Medium::Picture, media, 2 * SECOND).is_some());
        // The first live frame is two seconds of media later, at once.
        let live = anchors
            .note(Medium::Picture, media + 2 * SECOND, 2 * SECOND + 5 * MS)
            .expect("the estimate fell by two seconds");
        // It plays half a second after it arrived: 2.505 s, which is two
        // seconds of media after the first frame, plus the offset.
        assert_eq!(live[0], Some(505 * MS));
        // Frames up to 270 ms late do not move it.
        for step in 1..200 {
            let at = 2 * SECOND + 5 * MS + step * 33 * MS;
            let late = (step % 10) * 30 * MS;
            assert_eq!(
                anchors.note(
                    Medium::Picture,
                    media + 2 * SECOND + step * 33 * MS,
                    at + late
                ),
                None,
                "step {step}"
            );
        }
        // The camera's clock runs slow by a thousandth: every frame is a
        // little later, and after the window has passed the estimate
        // follows; the mapping moves when it has gone 80 ms.
        let mut moves = 0;
        for step in 200..6_000 {
            let media_at = media + 2 * SECOND + step * 33 * MS;
            let at = 2 * SECOND + 5 * MS + step * 33 * MS + (step - 200) * 33 * MS / 1_000;
            if anchors.note(Medium::Picture, media_at, at).is_some() {
                moves += 1;
            }
        }
        // 191 s at a thousandth is 191 ms of drift: two moves of 80 ms.
        assert_eq!(moves, 2);
    }

    /// A feed's first media time and later fragments may span the whole
    /// nonnegative input range. Comparing two resulting offsets stays exact
    /// even when their distance exceeds a signed integer.
    #[test]
    fn extreme_media_times_do_not_overflow_the_mapping_distance() {
        let mut anchors = Anchors::default();
        let first = i64::MAX / 2;
        assert_eq!(
            anchors.note(Medium::Picture, first, 0),
            Some([Some(nanos(DELAY)), None])
        );

        // Each later arrival begins a fresh least-delay window. The first
        // moves the applied offset below zero; the second moves it above zero
        // by more than i64::MAX.
        let below = first + (11 * SECOND - i64::MAX) + nanos(DELAY);
        assert_eq!(
            anchors.note(Medium::Picture, i64::MAX, 11 * SECOND),
            Some([Some(below), None])
        );
        let above = first + 22 * SECOND + nanos(DELAY);
        assert!(above.abs_diff(below) > i64::MAX.unsigned_abs());
        assert_eq!(
            anchors.note(Medium::Picture, 0, 22 * SECOND),
            Some([Some(above), None])
        );
    }

    /// A feed that starts over is placed anew by its next fragment.
    #[test]
    fn a_restarted_feed_is_placed_anew() {
        let mut anchors = Anchors::default();
        assert!(anchors.note(Medium::Sound, 10 * SECOND, SECOND).is_some());
        assert_eq!(
            anchors.note(Medium::Sound, 10 * SECOND + 16 * MS, SECOND + 16 * MS),
            None
        );
        anchors.restart(Medium::Sound);
        // Another run: media time starts elsewhere.
        let moved = anchors.note(Medium::Sound, 3 * SECOND, 20 * SECOND);
        assert_eq!(
            moved,
            Some([None, Some(3 * SECOND + 17 * SECOND + 500 * MS)])
        );
    }

    /// An `mp4a` sample entry with its `esds`: AAC-LC, one channel, 64 kHz.
    fn sound_entry() -> iris_mp4::init::SampleEntry {
        let mut entry = Vec::new();
        entry.extend_from_slice(&75_u32.to_be_bytes());
        entry.extend_from_slice(b"mp4a");
        entry.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        entry.extend_from_slice(&[0; 8]);
        entry.extend_from_slice(&[0, 1, 0, 16, 0, 0, 0, 0, 0xfa, 0, 0, 0]);
        entry.extend_from_slice(&39_u32.to_be_bytes());
        entry.extend_from_slice(b"esds");
        entry.extend_from_slice(&[0, 0, 0, 0]);
        entry.extend_from_slice(&[0x03, 0x19, 0, 1, 0]);
        entry.extend_from_slice(&[0x04, 0x11, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        entry.extend_from_slice(&[0x05, 0x02, 0x11, 0x08, 0x06, 0x01, 0x02]);
        iris_mp4::init::SampleEntry::audio(bytes::Bytes::from(entry), "mp4a.40.2".into())
    }

    /// A live fragment of one sample at `ticks`, as the daemon's WebSocket
    /// sends one.
    fn live_fragment(sequence: u32, ticks: u64, duration: u32) -> Vec<u8> {
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        let samples = [Sample {
            data: bytes::Bytes::from_static(&[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c]),
            duration,
            is_sync: true,
        }];
        let header = FragmentHeader {
            sequence,
            decode_time: ticks,
            wall: iris_core::time::WallUs::new(1_700_000_000_000_000),
        };
        let encoded = encode_fragment(&header, &samples).unwrap();
        let mut fragment = Vec::new();
        for slice in encoded.io_slices(&samples) {
            fragment.extend_from_slice(&slice);
        }
        fragment
    }

    /// One feed of the test below: its input, its pacer, and what its sink
    /// rendered, as media time and the running time of the rendering.
    struct TestFeed {
        src: gst_app::AppSrc,
        pacer: Pacer,
        rendered: Arc<Mutex<Vec<(i64, i64)>>>,
        /// Media time of the frame captured at the test's start, in ticks.
        media_start: u64,
        /// A frame's length in ticks.
        frame: u32,
        sent: u32,
    }

    impl TestFeed {
        fn new(pipeline: &gst::Pipeline, pairing: &Arc<Pairing>, medium: Medium) -> Self {
            let bin = gst::parse::bin_from_description(
                "appsrc name=src is-live=true stream-type=stream ! qtdemux \
                 ! identity name=shift ! fakesink name=sink sync=true async=false \
                 signal-handoffs=true",
                false,
            )
            .unwrap();
            let src = bin
                .by_name("src")
                .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
                .unwrap();
            let rendered = Arc::new(Mutex::new(Vec::new()));
            bin.by_name("sink").unwrap().connect("handoff", false, {
                let rendered = Arc::clone(&rendered);
                let clock = pipeline.clone();
                move |values| {
                    let buffer = values[1].get::<gst::Buffer>().unwrap();
                    let media = i64::try_from(buffer.pts().unwrap().nseconds()).unwrap();
                    let running =
                        i64::try_from(clock.current_running_time().unwrap().nseconds()).unwrap();
                    rendered.lock().unwrap().push((media, running));
                    None
                }
            });
            pipeline.add(&bin).unwrap();
            bin.sync_state_with_parent().unwrap();
            let pacer = pairing.pacer(
                medium,
                bin.by_name("shift").unwrap().static_pad("src").unwrap(),
            );
            src.push_buffer(gst::Buffer::from_slice(
                iris_mp4::init::build_init_segment(&sound_entry()).to_vec(),
            ))
            .unwrap();
            let (media_start, frame) = match medium {
                // Thirty pictures a second, 1,000 s into their run.
                Medium::Picture => (90_000_000, 3_000),
                // Audio frames of 16 ms, 5,000 s into another run.
                Medium::Sound => (450_000_000, 1_440),
            };
            Self {
                src,
                pacer,
                rendered,
                media_start,
                frame,
                sent: 0,
            }
        }

        /// Sends the next frame, as the tile's source does.
        fn send(&mut self) {
            let ticks = self.media_start + u64::from(self.sent) * u64::from(self.frame);
            let fragment = live_fragment(self.sent + 1, ticks, self.frame);
            self.pacer.payload(&fragment);
            self.src
                .push_buffer(gst::Buffer::from_slice(fragment))
                .unwrap();
            self.sent += 1;
        }

        /// When the frame after the last sent one was captured, in
        /// nanoseconds since the test's start.
        fn next_capture(&self) -> i64 {
            i64::from(self.sent) * i64::from(self.frame) * 100_000 / 9
        }

        /// How long after its capture each frame from `from` on was
        /// rendered: the least and the most, in nanoseconds, with `start`
        /// the running time of the test's start.
        fn delays(&self, start: i64, from: usize) -> (i64, i64, usize) {
            let media_zero = i64::try_from(u128::from(self.media_start) * 100_000 / 9).unwrap();
            let rendered = self.rendered.lock().unwrap();
            let delays: Vec<i64> = rendered
                .iter()
                .skip(from)
                .map(|(media, running)| (running - start) - (media - media_zero))
                .collect();
            (
                *delays.iter().min().unwrap(),
                *delays.iter().max().unwrap(),
                rendered.len(),
            )
        }
    }

    /// Two feeds of frames captured together, each on a timeline of its own:
    /// the picture's arrive 20 ms after their capture, the sound's in groups
    /// of five whose last is 20 ms old and whose first is 84 ms old. Every
    /// frame of both reaches its sink half a second after the least-delay
    /// arrival of its instant, so sound and picture of one instant are
    /// rendered together, and the mapping is set once.
    #[test]
    #[ignore = "requires the GStreamer MP4 demuxer (gst-plugins-good); `just display-test` runs it"]
    fn feeds_captured_together_reach_their_sinks_together() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.set_state(gst::State::Playing).unwrap();
        let engaged = Arc::new(AtomicBool::new(false));
        let pairing = Pairing::new(&pipeline, {
            let engaged = Arc::clone(&engaged);
            move || engaged.store(true, Ordering::Release)
        });
        let mut picture = TestFeed::new(&pipeline, &pairing, Medium::Picture);
        let mut sound = TestFeed::new(&pipeline, &pairing, Medium::Sound);
        let transit = 20 * MS;
        let running =
            || i64::try_from(pipeline.current_running_time().unwrap().nseconds()).unwrap();
        let start = running();
        while running() - start < 3 * SECOND {
            let now = running() - start;
            while picture.next_capture() + transit <= now {
                picture.send();
            }
            // A group goes out when its fifth frame is 20 ms old.
            while sound.next_capture() + 4 * 16 * MS + transit <= now {
                for _ in 0..5 {
                    sound.send();
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // What was sent in the last half second is still held.
        std::thread::sleep(Duration::from_millis(700));
        assert!(engaged.load(Ordering::Acquire));
        let (picture_least, picture_most, pictures) = picture.delays(start, 15);
        let (sound_least, sound_most, sounds) = sound.delays(start, 30);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            pictures > 80 && sounds > 170,
            "{pictures} pictures, {sounds} frames"
        );
        // Half a second after the least-delay arrival: 520 ms after capture.
        let wanted = transit + nanos(DELAY);
        for (what, delay) in [
            ("picture least", picture_least),
            ("picture most", picture_most),
            ("sound least", sound_least),
            ("sound most", sound_most),
        ] {
            assert!(
                (delay - wanted).abs() < 25 * MS,
                "{what}: rendered {} ms after capture, not near {} ms",
                delay / MS,
                wanted / MS
            );
        }
    }
}
