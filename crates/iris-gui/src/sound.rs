//! Sound in the focused live view: the pads that play a camera's audio feed
//! and its picture against one clock (`docs/research/13-audio.md` §10.4).
//!
//! The rule that places the two is [`SoundClock`], from `iris-client`: each
//! feed is held [`iris_client::sound_clock::DELAY`] after its least-delay
//! arrival, and the mapping moves past a threshold, for both feeds together.
//! Here it meets the pipeline. A [`Pacer`] tells its [`Pairing`] of each
//! payload its feed pushes. The pairing hands the clock the fragment's media
//! time with the pipeline's running time. It sets each offset the clock
//! returns on the pad after the feed's demuxer, which starts its segment at
//! the feed's first fragment.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use iris_client::live;
use iris_client::sound_clock::{Engagement, Medium, SoundClock};
use tracing::debug;

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

/// The picture and the sound of one camera, played against the clock of
/// the pipeline their branches are in.
pub struct Pairing {
    /// What tells the running time: the pipeline.
    pipeline: gst::Element,
    state: Mutex<State>,
}

impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    /// The place of both feeds on the clock, and whether it is in force.
    clock: SoundClock,
    /// The pad after the picture's demuxer, where the picture's offset is
    /// set.
    video_pad: Option<gst::Pad>,
    /// The pad after the sound's demuxer, where the sound's offset is set.
    audio_pad: Option<gst::Pad>,
    /// Called once, when the mapping comes into force.
    on_engage: Option<Box<dyn FnOnce() + Send>>,
}

impl Pairing {
    /// A pairing on the clock of `pipeline`. `on_engage` runs once, when
    /// the sound's first fragment arrives: until then the picture is not
    /// held, and a camera without sound never holds it.
    pub fn new(pipeline: &gst::Pipeline, on_engage: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Arc::new(Self {
            pipeline: pipeline.clone().upcast(),
            state: Mutex::new(State {
                on_engage: Some(Box::new(on_engage)),
                ..State::default()
            }),
        })
    }

    /// The pacer of one feed, whose frames pass `pad` after its demuxer.
    pub fn pacer(self: &Arc<Self>, feed: Medium, pad: gst::Pad) -> Pacer {
        let mut state = self.state.lock().expect("pairing lock");
        match feed {
            Medium::Video => state.video_pad = Some(pad),
            Medium::Audio => state.audio_pad = Some(pad),
        }
        drop(state);
        Pacer {
            pairing: Arc::clone(self),
            feed,
        }
    }

    fn restart(&self, feed: Medium) {
        self.state.lock().expect("pairing lock").clock.restart(feed);
    }

    fn fragment(&self, feed: Medium, payload: &[u8]) {
        let Some(media) = live::media_time(payload) else {
            return;
        };
        let Some(running) = self.pipeline.current_running_time() else {
            return;
        };
        let mut state = self.state.lock().expect("pairing lock");
        // The clock answers with the offsets to set: both feeds' on the
        // sound's first fragment, and after it both feeds' when the mapping
        // moves.
        let Some(placement) = state.clock.fragment(feed, media, Duration::from(running)) else {
            return;
        };
        let placed = [
            (&state.video_pad, placement.video_offset_ns),
            (&state.audio_pad, placement.audio_offset_ns),
        ];
        for (pad, offset) in placed {
            if let (Some(pad), Some(offset)) = (pad, offset) {
                pad.set_offset(offset);
            }
        }
        // The picture is held from here on, with its mapping in place.
        if placement.engaged == Engagement::Engaged
            && let Some(on_engage) = state.on_engage.take()
        {
            on_engage();
        }
        debug!(
            picture_ms = placement.video_offset_ns.map(|offset| offset / 1_000_000),
            sound_ms = placement.audio_offset_ns.map(|offset| offset / 1_000_000),
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
    use std::sync::atomic::AtomicUsize;

    use iris_client::sound_clock::DELAY;

    use super::*;

    const NANOS_PER_MS: i64 = 1_000_000;

    const NANOS_PER_SECOND: i64 = 1_000_000_000;

    fn nanos(duration: Duration) -> i64 {
        i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
    }

    /// No test plays a camera's audio on the machine it runs on.
    #[test]
    fn tests_discard_audio() {
        assert!(silent());
        mute();
        assert!(silent());
    }

    /// The start of an init segment of the sound: a payload that is no
    /// fragment, since it starts with no `prft` box.
    const SOUND_INIT: &[u8] = b"\0\0\0\x1cftypiso6\0\0\0\0iso6cmfcmp41";

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

    /// A pairing on a playing pipeline, with a pad and a pacer for each feed
    /// and a count of the times the pairing engaged.
    struct Paired {
        pipeline: gst::Pipeline,
        engagements: Arc<AtomicUsize>,
        picture_pad: gst::Pad,
        sound_pad: gst::Pad,
        picture: Pacer,
        sound: Pacer,
    }

    impl Paired {
        fn new() -> Self {
            gst::init().unwrap();
            let pipeline = gst::Pipeline::new();
            pipeline.set_state(gst::State::Playing).unwrap();
            let (_, state, _) = pipeline.state(gst::ClockTime::from_seconds(5));
            assert_eq!(state, gst::State::Playing);
            let engagements = Arc::new(AtomicUsize::new(0));
            let pairing = Pairing::new(&pipeline, {
                let engagements = Arc::clone(&engagements);
                move || {
                    engagements.fetch_add(1, Ordering::AcqRel);
                }
            });
            let pad = || gst::Pad::builder(gst::PadDirection::Src).build();
            let (picture_pad, sound_pad) = (pad(), pad());
            let picture = pairing.pacer(Medium::Video, picture_pad.clone());
            let sound = pairing.pacer(Medium::Audio, sound_pad.clone());
            Self {
                pipeline,
                engagements,
                picture_pad,
                sound_pad,
                picture,
                sound,
            }
        }

        fn running_ns(&self) -> i64 {
            i64::try_from(self.pipeline.current_running_time().unwrap().nseconds()).unwrap()
        }

        fn engagements(&self) -> usize {
            self.engagements.load(Ordering::Acquire)
        }

        /// The picture's first two fragments, 33 ms of media apart, which
        /// arrive at once.
        fn picture_alone(&self) {
            self.picture.payload(&prft(90_000_000));
            self.picture.payload(&prft(90_003_000));
        }

        /// The picture's first fragments, then the sound's init segment and
        /// first fragment, on another timeline than the picture's; returns
        /// the running time before that fragment arrived.
        fn sounds_first_fragment(&self) -> i64 {
            self.picture_alone();
            self.sound.payload(SOUND_INIT);
            let before_ns = self.running_ns();
            self.sound.payload(&prft(450_000_000));
            before_ns
        }

        /// The sound's second fragment, 16 ms after its first.
        fn sounds_later_fragment(&self) {
            std::thread::sleep(Duration::from_millis(16));
            self.sound.payload(&prft(450_001_440));
        }

        /// The sound starts over on another run 30 ms on, and its next
        /// fragment arrives; returns the running time before it arrived.
        fn sound_starts_over(&self) -> i64 {
            std::thread::sleep(Duration::from_millis(30));
            self.sound.restart();
            let before_ns = self.running_ns();
            self.sound.payload(&prft(9_000));
            before_ns
        }

        /// The picture starts over 30 ms on, and its next fragment arrives;
        /// returns the running time before it arrived.
        fn picture_starts_over(&self) -> i64 {
            std::thread::sleep(Duration::from_millis(30));
            self.picture.restart();
            let before_ns = self.running_ns();
            self.picture.payload(&prft(18_000));
            before_ns
        }

        /// Checks that the offset of `pad` is that of a first fragment that
        /// arrived after `before_ns`: the running time of its arrival plus
        /// the delay. The demuxer counts a feed's running time from its
        /// first fragment, so the offset plays that fragment half a second
        /// after it arrived.
        fn assert_placed_now(&self, pad: &gst::Pad, before_ns: i64) {
            let offset = pad.offset();
            let after_ns = self.running_ns();
            assert!(
                ((before_ns + nanos(DELAY))..=(after_ns + nanos(DELAY))).contains(&offset),
                "offset {offset} outside {before_ns}..={after_ns} plus the delay"
            );
        }
    }

    impl Drop for Paired {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// The picture's fragments before the sound's first set no offset on
    /// either pad.
    #[test]
    fn the_picture_alone_sets_no_offset() {
        let paired = Paired::new();
        paired.picture_alone();
        assert_eq!(
            (paired.picture_pad.offset(), paired.sound_pad.offset()),
            (0, 0)
        );
    }

    /// The picture's fragments before the sound's first do not engage the
    /// pairing.
    #[test]
    fn the_picture_alone_does_not_engage_the_pairing() {
        let paired = Paired::new();
        paired.picture_alone();
        assert_eq!(paired.engagements(), 0);
    }

    /// An init segment of the sound is not a fragment: it does not engage
    /// the pairing.
    #[test]
    fn an_init_segment_of_the_sound_does_not_engage_the_pairing() {
        let paired = Paired::new();
        paired.picture_alone();
        paired.sound.payload(SOUND_INIT);
        assert_eq!(paired.engagements(), 0);
    }

    /// An init segment of the sound is not a fragment: it sets no offset on
    /// the sound's pad.
    #[test]
    fn an_init_segment_of_the_sound_sets_no_offset() {
        let paired = Paired::new();
        paired.picture_alone();
        paired.sound.payload(SOUND_INIT);
        assert_eq!(paired.sound_pad.offset(), 0);
    }

    #[test]
    fn the_sounds_first_fragment_engages_the_pairing() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        assert_eq!(paired.engagements(), 1);
    }

    /// The offset of the sound's first fragment, at 5,000 s of media time,
    /// plays it half a second after its arrival.
    #[test]
    fn the_sounds_first_fragment_plays_half_a_second_after_its_arrival() {
        let paired = Paired::new();
        let before_ns = paired.sounds_first_fragment();
        paired.assert_placed_now(&paired.sound_pad, before_ns);
    }

    /// The sound's first fragment puts the picture's offset in force too:
    /// the picture's first fragment arrived a moment earlier, and its second
    /// 33 ms of media after it.
    #[test]
    fn the_sounds_first_fragment_puts_the_pictures_offset_in_force() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        let (picture, sound) = (paired.picture_pad.offset(), paired.sound_pad.offset());
        assert!(
            picture > 0 && picture <= sound,
            "picture {picture}, sound {sound}"
        );
        assert!(
            sound - picture < 200 * NANOS_PER_MS,
            "picture {picture}, sound {sound}"
        );
    }

    #[test]
    fn a_later_fragment_of_the_sound_leaves_its_offset() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        let settled = paired.sound_pad.offset();
        paired.sounds_later_fragment();
        assert!(
            (paired.sound_pad.offset() - settled).abs() <= NANOS_PER_MS,
            "the estimate held"
        );
    }

    /// The pairing engages on the sound's first fragment alone: not on its
    /// later fragments, nor when it starts over.
    #[test]
    fn the_pairing_engages_once() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        paired.sounds_later_fragment();
        assert_eq!(paired.engagements(), 1);
        paired.sound_starts_over();
        assert_eq!(paired.engagements(), 1);
    }

    /// A sound that starts over is placed by its next fragment, half a
    /// second after that fragment's arrival.
    #[test]
    fn a_sound_that_starts_over_is_placed_by_its_next_fragment() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        let settled = paired.sound_pad.offset();
        paired.sounds_later_fragment();
        let before_ns = paired.sound_starts_over();
        paired.assert_placed_now(&paired.sound_pad, before_ns);
        assert!(paired.sound_pad.offset() > settled + 25 * NANOS_PER_MS);
    }

    /// A picture that starts over is placed by its next fragment, half a
    /// second after that fragment's arrival.
    #[test]
    fn a_picture_that_starts_over_is_placed_by_its_next_fragment() {
        let paired = Paired::new();
        paired.sounds_first_fragment();
        paired.sounds_later_fragment();
        paired.sound_starts_over();
        let before_ns = paired.picture_starts_over();
        paired.assert_placed_now(&paired.picture_pad, before_ns);
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

    /// A frame a sink rendered: its media time and the running time of the
    /// rendering.
    #[derive(Clone, Copy, Debug)]
    struct Rendered {
        media_ns: i64,
        /// The running time of the rendering, in nanoseconds.
        running_ns: i64,
    }

    /// How long after their capture a feed's frames were rendered, and how
    /// many frames its sink rendered in all.
    #[derive(Clone, Copy, Debug)]
    struct Delays {
        least_ns: i64,
        most_ns: i64,
        /// The frames the sink rendered.
        rendered: usize,
    }

    /// One feed of the test below: its input, its pacer, and what its sink
    /// rendered.
    struct TestFeed {
        src: gst_app::AppSrc,
        pacer: Pacer,
        rendered: Arc<Mutex<Vec<Rendered>>>,
        /// Media time of the frame captured at the test's start.
        media_start_ticks: u64,
        frame_ticks: u32,
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
                    let media_ns = i64::try_from(buffer.pts().unwrap().nseconds()).unwrap();
                    let running_ns =
                        i64::try_from(clock.current_running_time().unwrap().nseconds()).unwrap();
                    rendered.lock().unwrap().push(Rendered {
                        media_ns,
                        running_ns,
                    });
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
            let (media_start_ticks, frame_ticks) = match medium {
                // Thirty pictures a second, 1,000 s into their run.
                Medium::Video => (90_000_000, 3_000),
                // Audio frames of 16 ms, 5,000 s into another run.
                Medium::Audio => (450_000_000, 1_440),
            };
            Self {
                src,
                pacer,
                rendered,
                media_start_ticks,
                frame_ticks,
                sent: 0,
            }
        }

        /// Sends the next frame, as the tile's source does.
        fn send(&mut self) {
            let ticks = self.media_start_ticks + u64::from(self.sent) * u64::from(self.frame_ticks);
            let fragment = live_fragment(self.sent + 1, ticks, self.frame_ticks);
            self.pacer.payload(&fragment);
            self.src
                .push_buffer(gst::Buffer::from_slice(fragment))
                .unwrap();
            self.sent += 1;
        }

        /// When the frame after the last sent one was captured, in
        /// nanoseconds since the test's start.
        fn next_capture_ns(&self) -> i64 {
            i64::from(self.sent) * i64::from(self.frame_ticks) * 100_000 / 9
        }

        /// How long after its capture each frame from `from` on was
        /// rendered, the least and the most, with `start_ns` the running
        /// time of the test's start in nanoseconds.
        fn delays(&self, start_ns: i64, from: usize) -> Delays {
            let media_zero_ns =
                i64::try_from(u128::from(self.media_start_ticks) * 100_000 / 9).unwrap();
            let rendered = self.rendered.lock().unwrap();
            let delays_ns: Vec<i64> = rendered
                .iter()
                .skip(from)
                .map(|frame| (frame.running_ns - start_ns) - (frame.media_ns - media_zero_ns))
                .collect();
            Delays {
                least_ns: *delays_ns.iter().min().unwrap(),
                most_ns: *delays_ns.iter().max().unwrap(),
                rendered: rendered.len(),
            }
        }
    }

    /// Two feeds of frames captured together, on different timelines. The
    /// picture's frames arrive 20 ms after their capture. The sound's arrive in
    /// groups of five, whose last is 20 ms old and whose first is 84 ms old.
    /// With the mapping in force, every frame of both reaches its sink half a
    /// second after the least-delay arrival of its instant. Sound and picture
    /// of one instant are then rendered together.
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
        let mut picture = TestFeed::new(&pipeline, &pairing, Medium::Video);
        let mut sound = TestFeed::new(&pipeline, &pairing, Medium::Audio);
        let transit_ns = 20 * NANOS_PER_MS;
        let running_ns =
            || i64::try_from(pipeline.current_running_time().unwrap().nseconds()).unwrap();
        let start_ns = running_ns();
        while running_ns() - start_ns < 3 * NANOS_PER_SECOND {
            let now_ns = running_ns() - start_ns;
            while picture.next_capture_ns() + transit_ns <= now_ns {
                picture.send();
            }
            // A group goes out when its fifth frame is 20 ms old.
            while sound.next_capture_ns() + 4 * 16 * NANOS_PER_MS + transit_ns <= now_ns {
                for _ in 0..5 {
                    sound.send();
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // What was sent in the last half second is still held.
        std::thread::sleep(Duration::from_millis(700));
        assert!(engaged.load(Ordering::Acquire));
        let pictures = picture.delays(start_ns, 15);
        let sounds = sound.delays(start_ns, 30);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            pictures.rendered > 80 && sounds.rendered > 170,
            "{} pictures, {} frames",
            pictures.rendered,
            sounds.rendered
        );
        // Half a second after the least-delay arrival: 520 ms after capture.
        let wanted_ns = transit_ns + nanos(DELAY);
        for (what, delay_ns) in [
            ("picture least", pictures.least_ns),
            ("picture most", pictures.most_ns),
            ("sound least", sounds.least_ns),
            ("sound most", sounds.most_ns),
        ] {
            assert!(
                (delay_ns - wanted_ns).abs() < 25 * NANOS_PER_MS,
                "{what}: rendered {} ms after capture, not near {} ms",
                delay_ns / NANOS_PER_MS,
                wanted_ns / NANOS_PER_MS
            );
        }
    }
}
