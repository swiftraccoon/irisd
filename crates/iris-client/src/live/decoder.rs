//! The live decoder: one message of the live feed in, the decoder
//! configuration of an init segment or the one sample of a fragment out.

use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::decoder_config::DecoderConfig;
use iris_mp4::reader;

use crate::live::{LiveError, LiveMessage, live_message};

/// One sample of a live fragment: where its bytes lie in the message that
/// carried it, and its timing. The boxes around the sample are read where
/// they lie and none of the message is copied, so a viewer hands its decoder
/// the [`len`](LiveSample::len) bytes from [`offset`](LiveSample::offset) of
/// the buffer it holds. Whether the platform's sample buffer copies them is
/// unverified (`docs/research/15-apple-clients.md` §10).
///
/// An HEVC sample is the NAL units of one access unit, each behind a
/// four-byte big-endian length, the one length size
/// [`iris_mp4::decoder_config::parse_hvcc`] admits. A sample of the daemon's
/// `hvc1` streams holds no parameter set: the configuration holds them
/// (`docs/research/15-apple-clients.md` §3.6). [`DecoderConfig::parse`]
/// reads an `hev1` stream too when its `hvcC` holds the three sets
/// (`docs/research/15-apple-clients.md` §11), and a sample of such a stream
/// may carry them in band. A format description built from the
/// configuration's parameter sets takes a NAL unit header length of four,
/// the samples' length size (`nalUnitHeaderLength` of
/// `CMVideoFormatDescriptionCreateFromHEVCParameterSets`); a description with
/// any other length misreads every sample. An AAC sample is one raw frame,
/// without an ADTS header.
///
/// Only [`LiveDecoder::decode`] makes one, so each lies inside the message
/// it was read from. The example on [`LiveDecoder`] reads one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LiveSample {
    /// The offset of the sample's first byte from the message's first byte.
    offset: u32,
    /// The sample's length in bytes, never zero.
    len: u32,
    /// The fragment's `tfdt`, in 90 kHz ticks.
    decode_time: MediaTicks,
    /// The duration in the fragment's `trun`, in 90 kHz ticks.
    duration: MediaTicks,
    /// Whether the reader found a sync sample.
    is_sync: bool,
    /// The wall-clock time of the fragment's `prft` box.
    wall: Option<WallUs>,
}

impl LiveSample {
    /// The offset of the sample's first byte from the start of the message,
    /// whose first byte is its type byte. The example on [`LiveDecoder`]
    /// reads it.
    #[must_use]
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// The sample's length in bytes, never zero. The sum of
    /// [`offset`](LiveSample::offset) and `len` is at most the message's
    /// length, and computes in `u32` without overflow. The example on
    /// [`LiveDecoder`] reads it.
    #[must_use]
    #[expect(
        clippy::len_without_is_empty,
        reason = "a sample is never empty, so `is_empty` would always be false"
    )]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// The decode time of the sample, the fragment's `tfdt`, in 90 kHz ticks,
    /// and its presentation time as well. The daemon writes no composition
    /// offsets, and the reader passes over any that a fragment holds. A
    /// `CMSampleBuffer` therefore takes this time as both its decode and its
    /// presentation timestamp. A viewer times the frame by it and never by a
    /// sum of durations, since a gap in the feed shows only here
    /// (`docs/research/15-apple-clients.md` §3.1). The example on
    /// [`LiveDecoder`] reads it.
    #[must_use]
    pub fn decode_time(&self) -> MediaTicks {
        self.decode_time
    }

    /// The sample's duration, from the fragment's `trun`, in 90 kHz ticks.
    /// The example on [`LiveDecoder`] reads it.
    #[must_use]
    pub fn duration(&self) -> MediaTicks {
        self.duration
    }

    /// Whether the sample is a sync sample, which decodes without the samples
    /// before it. The daemon marks every audio frame one. The example on
    /// [`LiveDecoder`] reads it.
    #[must_use]
    pub fn is_sync(&self) -> bool {
        self.is_sync
    }

    /// The wall-clock time the fragment's `prft` box gives the sample, by the
    /// daemon's clock; none for a fragment without the box. It need not rise
    /// from one sample to the next: frames received together can carry times
    /// up to a microsecond apart in either order
    /// (`docs/research/15-apple-clients.md` §3.6), so a viewer orders and
    /// times frames by [`decode_time`](LiveSample::decode_time). The example
    /// on [`LiveDecoder`] reads it.
    #[must_use]
    pub fn wall(&self) -> Option<WallUs> {
        self.wall
    }
}

/// What one message of the live feed gives a decoder. Its variants are a
/// closed set, which every consumer handles whole. The `Debug` output of a
/// configuration shows its parameter sets or its `AudioSpecificConfig`, a
/// stream's public configuration. The example on [`LiveDecoder`] matches
/// both.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LiveItem {
    /// An init segment's decoder configuration, which the samples after it
    /// decode with. For an HEVC stream it holds the parameter sets and the
    /// picture size, and the samples carry four-byte NAL unit lengths
    /// ([`LiveSample`]). For an AAC stream it holds the `AudioSpecificConfig`,
    /// the sampling rate, and the channel count.
    Config(DecoderConfig),
    /// The one sample of a fragment, placed in the message.
    Sample(LiveSample),
}

/// Reads the messages of one live feed in order, for a viewer that decodes
/// the feed itself, and gives it only what its decoder can decode. An init
/// segment gives the [`LiveItem::Config`] the decoder is configured with. A
/// fragment gives the [`LiveItem::Sample`] it holds, placed in the message.
///
/// After each configuration the decoder awaits a sync sample. It refuses each
/// sample before it with [`LiveError::AwaitingSync`], which the viewer drops.
/// After a fragment it refused, it awaits one again, since the samples after
/// that fragment may depend on it. While it holds no configuration, after
/// [`LiveDecoder::new`], [`LiveDecoder::reset`], or an init segment it
/// refused, it refuses every fragment with [`LiveError::NoConfig`].
///
/// A viewer drops a fragment refused with [`LiveError::Fragment`],
/// [`LiveError::DecodeTime`], [`LiveError::SampleCount`], or `AwaitingSync`,
/// and reads on. It ends the session on [`LiveError::Config`] and
/// `NoConfig`, as on a message whose framing it refuses, since the next
/// session opens with an init segment. [`LiveDecoder::decode`] says this of
/// each refusal. Nothing here panics on a message, and a sample it gives lies
/// inside its message whatever the message holds; the fuzz target
/// `live_sample` checks both.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// use std::slice;
///
/// use iris_client::live::{self, LiveDecoder, LiveError, LiveItem};
/// use iris_core::time::{MediaTicks, WallUs};
/// use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
/// # use iris_mp4::boxes::BoxWriter;
/// # use iris_mp4::init::{SampleEntry, build_init_segment};
///
/// // `init` is the init message of an audio feed: AAC-LC at 64 kHz in one
/// // channel.
/// # let mut w = BoxWriter::default();
/// # let mp4a = w.begin(b"mp4a");
/// # w.zeros(6);
/// # w.u16(1); // data_reference_index
/// # w.zeros(8);
/// # w.u16(1); // channelcount
/// # w.u16(16); // samplesize
/// # w.zeros(4);
/// # w.u32(64_000 << 16); // samplerate
/// # let esds = w.begin_full(b"esds", 0, 0);
/// # w.bytes(&[0x03, 22, 0, 1, 0]); // ES_Descriptor: ES_ID 1, no flags
/// # w.bytes(&[0x04, 17, 0x40, 0x15]); // DecoderConfigDescriptor: MPEG-4 audio
/// # w.zeros(11); // bufferSizeDB, maxBitrate, avgBitrate
/// # w.bytes(&[0x05, 2, 0x11, 0x08]); // DecoderSpecificInfo
/// # w.end(esds);
/// # w.end(mp4a);
/// # let entry = SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into());
/// # let mut init = vec![live::MSG_INIT];
/// # init.extend_from_slice(&build_init_segment(&entry));
///
/// // The message of one audio frame, as the daemon builds it.
/// let frame = Sample {
///     data: vec![0x21; 300].into(),
///     duration: 1440,
///     is_sync: true,
/// };
/// let header = FragmentHeader {
///     sequence: 1,
///     decode_time: 90_000,
///     wall: WallUs::new(0),
/// };
/// let encoded = encode_fragment(&header, slice::from_ref(&frame))?;
/// let mut message = vec![live::MSG_FRAGMENT];
/// message.extend_from_slice(&encoded.prefix);
/// message.extend_from_slice(&frame.data);
/// message.extend_from_slice(&encoded.trailer);
///
/// let mut decoder = LiveDecoder::new();
/// assert_eq!(decoder.decode(&message), Err(LiveError::NoConfig));
/// assert!(matches!(decoder.decode(&init)?, LiveItem::Config(_)));
/// let LiveItem::Sample(sample) = decoder.decode(&message)? else {
///     return Err("one audio frame is one sample".into());
/// };
/// // The sample is a range of the message the caller holds.
/// let start = usize::try_from(sample.offset())?;
/// let end = start + usize::try_from(sample.len())?;
/// assert_eq!(message[start..end], frame.data[..]);
/// // A sync sample, timed by its decode time, and labelled by its wall time.
/// assert!(sample.is_sync(), "every audio frame is a sync sample");
/// assert_eq!(sample.decode_time(), MediaTicks::new(90_000));
/// assert_eq!(sample.duration(), MediaTicks::new(1440));
/// assert_eq!(sample.wall(), Some(WallUs::new(0)));
///
/// // The next session opens with an init segment: until it comes, the decoder
/// // refuses every fragment.
/// decoder.reset();
/// assert_eq!(decoder.decode(&message), Err(LiveError::NoConfig));
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LiveDecoder {
    state: State,
}

/// What a [`LiveDecoder`] holds between messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum State {
    /// No configuration: before the first init segment, after a reset, and
    /// after an init segment that yields none.
    #[default]
    Unconfigured,
    /// A configuration, and no sync sample since it or since a refused
    /// fragment.
    AwaitingSync,
    /// A configuration, and a sync sample since it: each sample decodes.
    Decoding,
}

impl LiveDecoder {
    /// A decoder that holds no configuration, as at the start of a session.
    /// The example on [`LiveDecoder`] makes one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Unconfigured,
        }
    }

    /// Reads one message of the live feed as the transport received it, type
    /// byte included. The example on [`LiveDecoder`] reads an init segment
    /// and a fragment.
    ///
    /// An init segment gives its configuration ([`DecoderConfig::parse`]),
    /// which replaces the one before, and the decoder then awaits a sync
    /// sample. A fragment of one sample ([`iris_mp4::reader::parse_fragment`])
    /// gives that sample, placed by its offset and length in `message`, unless
    /// it is not a sync sample and the decoder awaits one.
    ///
    /// # Errors
    ///
    /// - [`LiveError::Empty`] or [`LiveError::UnknownType`] for a message that
    ///   [`live_message`] refuses, which leaves the decoder as it was. The
    ///   viewer ends the session, as on any message whose framing breaks the
    ///   protocol ([`crate::live`]).
    /// - [`LiveError::Config`] for an init segment that yields no
    ///   configuration, after which the decoder holds none, since the
    ///   fragments after the segment belong to its stream. The viewer ends the
    ///   session, whose next one opens with an init segment.
    /// - [`LiveError::NoConfig`] for a fragment while the decoder holds no
    ///   configuration, refused before it is read. The viewer ends the session,
    ///   as after a refused init segment, since no fragment decodes until the
    ///   next one.
    /// - [`LiveError::Fragment`] for a fragment that does not parse, whose
    ///   sample is empty, or whose sample would not lie inside `message`.
    ///   [`LiveError::SampleCount`] for a fragment of other than one sample,
    ///   and [`LiveError::DecodeTime`] for one whose decode time is past the
    ///   range of [`MediaTicks`]. After any of them the decoder awaits a sync
    ///   sample, and the viewer drops the fragment and reads on.
    /// - [`LiveError::AwaitingSync`] for a fragment that passes those checks
    ///   and whose sample is not a sync sample while the decoder awaits one.
    ///   The viewer drops it and reads on.
    pub fn decode(&mut self, message: &[u8]) -> Result<LiveItem, LiveError> {
        match live_message(message)? {
            LiveMessage::Init(init) => {
                // Until the segment is read, no configuration describes the
                // fragments after it.
                self.state = State::Unconfigured;
                let config = DecoderConfig::parse(init)?;
                self.state = State::AwaitingSync;
                Ok(LiveItem::Config(config))
            }
            LiveMessage::Fragment(_) if self.state == State::Unconfigured => {
                Err(LiveError::NoConfig)
            }
            LiveMessage::Fragment(fragment) => {
                let sample = match one_sample(message, fragment) {
                    Ok(sample) => sample,
                    Err(refusal) => {
                        // The samples after a refused fragment may depend on
                        // it.
                        self.state = State::AwaitingSync;
                        return Err(refusal);
                    }
                };
                if self.state == State::AwaitingSync && !sample.is_sync() {
                    return Err(LiveError::AwaitingSync);
                }
                self.state = State::Decoding;
                Ok(LiveItem::Sample(sample))
            }
        }
    }

    /// Forgets the configuration, so that the decoder holds none, as after
    /// [`LiveDecoder::new`]. A viewer resets it as each session opens, whose
    /// first message is an init segment. The example on [`LiveDecoder`]
    /// resets one.
    pub fn reset(&mut self) {
        self.state = State::Unconfigured;
    }
}

/// The one sample of `fragment`, the payload of `message` after its type
/// byte, as [`LiveDecoder::decode`] gives it.
fn one_sample(message: &[u8], fragment: &[u8]) -> Result<LiveSample, LiveError> {
    let info = reader::parse_fragment(fragment).ok_or(LiveError::Fragment)?;
    // The sample tables hold a size and a duration for each sample.
    let (&[len], &[duration]) = (
        info.sample_sizes.as_slice(),
        info.sample_durations.as_slice(),
    ) else {
        return Err(LiveError::SampleCount(info.sample_count));
    };
    // A sample of no bytes holds no picture and no sound; whether a block
    // buffer takes it is unverified. Taken as a sync sample, it would let the
    // samples after it reach a decoder that never had a picture.
    if len == 0 {
        return Err(LiveError::Fragment);
    }
    let offset = sample_offset(message, info.payload_offset, len).ok_or(LiveError::Fragment)?;
    let decode_time = i64::try_from(info.decode_time).map_err(LiveError::DecodeTime)?;
    Ok(LiveSample {
        offset,
        len,
        decode_time: MediaTicks::new(decode_time),
        duration: MediaTicks::new(i64::from(duration)),
        is_sync: info.starts_with_sync,
        wall: info.wall,
    })
}

/// The offset in `message` of a sample of `len` bytes that starts
/// `payload_offset` bytes into the fragment after the message's type byte.
/// The payload offset counts from the fragment's first byte, as
/// [`reader::parse_fragment`] counts it. None when the sample would end past
/// the message or past `u32::MAX`.
fn sample_offset(message: &[u8], payload_offset: u64, len: u32) -> Option<u32> {
    let offset = u32::try_from(payload_offset).ok()?.checked_add(1)?;
    let end = offset.checked_add(len)?;
    let (start, end) = (usize::try_from(offset).ok()?, usize::try_from(end).ok()?);
    message.get(start..end).map(|_| offset)
}

#[cfg(test)]
mod tests {
    use iris_mp4::boxes::BoxWriter;
    use iris_mp4::decoder_config::DecoderConfigError;
    use iris_mp4::fragment::{FragmentHeader, Sample, TRAILER_LEN, encode_fragment};
    use iris_mp4::init::{SampleEntry, build_init_segment};

    use super::*;
    use crate::live::{MSG_FRAGMENT, MSG_INIT};

    /// A VPS, an SPS, and a PPS: each a NAL unit header of its type and a
    /// byte of payload, which is all the `hvcC` reader looks at.
    const PARAMETER_SETS: [[u8; 3]; 3] =
        [[32 << 1, 1, 0x0c], [33 << 1, 1, 0x01], [34 << 1, 1, 0xc1]];

    /// The byte every fixture sample is made of.
    const PAYLOAD: u8 = 0xab;

    /// The length of a fixture video frame.
    const PAYLOAD_LEN: usize = 1_000;

    /// A decode time past 32 bits, which only a version-1 `tfdt` holds.
    const DECODE_TIME: u64 = 9_000_003_000;

    /// The wall time every fixture's `prft` carries; it survives the round
    /// trip through the box's NTP timestamp exactly.
    const WALL: WallUs = WallUs::new(1_763_294_400_123_456);

    /// A visual sample entry of box type `kind`, `width` by `height`, whose
    /// `hvcC` holds the three `PARAMETER_SETS` with four-byte NAL lengths.
    fn visual_entry(kind: [u8; 4], width: u16, height: u16) -> SampleEntry {
        let mut w = BoxWriter::default();
        let entry = w.begin(&kind);
        w.zeros(6); // reserved
        w.u16(1); // data_reference_index
        w.zeros(16); // pre_defined and reserved
        w.u16(width);
        w.u16(height);
        w.u32(0x0048_0000); // horizresolution, 72 dpi
        w.u32(0x0048_0000); // vertresolution
        w.u32(0); // reserved
        w.u16(1); // frame_count
        w.zeros(32); // compressorname
        w.u16(0x18); // depth
        w.i16(-1); // pre_defined
        let hvcc = w.begin(b"hvcC");
        w.u8(1); // configurationVersion
        w.bytes(&[0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a]); // profile, tier, and level
        w.bytes(&[0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0]); // through avgFrameRate
        w.u8(0x0f); // lengthSizeMinusOne: four-byte NAL lengths
        w.u8(3); // numOfArrays
        for nal in PARAMETER_SETS {
            w.u8(0x80 | (nal[0] >> 1)); // array_completeness and the NAL unit type
            w.u16(1); // numNalus
            w.u16(3); // nalUnitLength
            w.bytes(&nal);
        }
        w.end(hvcc);
        w.end(entry);
        SampleEntry::new(w.into_bytes(), "hvc1.1.6.L120.90".into(), width, height)
    }

    /// The `hvc1` sample entry of a stream `width` by `height`.
    fn hevc_entry(width: u16, height: u16) -> SampleEntry {
        visual_entry(*b"hvc1", width, height)
    }

    /// Checks that `item` is what the decoder reads from the init segment of
    /// `hevc_entry(width, height)`: the three `PARAMETER_SETS` and the
    /// picture size.
    #[track_caller]
    fn assert_hevc_config(item: Result<LiveItem, LiveError>, width: u16, height: u16) {
        match item {
            Ok(LiveItem::Config(DecoderConfig::Hevc(config))) => {
                assert_eq!(config.parameter_sets(), PARAMETER_SETS);
                assert_eq!((config.width(), config.height()), (width, height));
            }
            other => panic!("no HEVC configuration: {other:?}"),
        }
    }

    /// An `mp4a` sample entry of AAC-LC at 64 kHz in one channel, the
    /// `AudioSpecificConfig` `11 08`. Its `esds` is not the daemon's: it
    /// gives `ES_ID` 1 and no `SLConfigDescriptor`, where retina's
    /// `make_sample_entry`, which writes the daemon's, gives `ES_ID` 0 and
    /// one; the tests of `crates/iris-mp4/src/decoder_config.rs` read that
    /// layout.
    fn aac_entry() -> SampleEntry {
        let mut w = BoxWriter::default();
        let entry = w.begin(b"mp4a");
        w.zeros(6); // reserved
        w.u16(1); // data_reference_index
        w.zeros(8); // reserved
        w.u16(1); // channelcount
        w.u16(16); // samplesize
        w.zeros(4); // pre_defined and reserved
        w.u32(64_000 << 16); // samplerate
        let esds = w.begin_full(b"esds", 0, 0);
        w.bytes(&[0x03, 22, 0, 1, 0]); // ES_Descriptor: ES_ID 1, no flags
        w.bytes(&[0x04, 17, 0x40, 0x15]); // DecoderConfigDescriptor: MPEG-4 audio
        w.zeros(11); // bufferSizeDB, maxBitrate, avgBitrate
        w.bytes(&[0x05, 2, 0x11, 0x08]); // DecoderSpecificInfo: the AudioSpecificConfig
        w.end(esds);
        w.end(entry);
        SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into())
    }

    /// An init message as the daemon sends one: the type byte and the init
    /// segment of `entry`.
    fn init_message(entry: &SampleEntry) -> Vec<u8> {
        let mut message = vec![MSG_INIT];
        message.extend_from_slice(&build_init_segment(entry));
        message
    }

    /// The header of every fixture fragment at `decode_time`.
    fn header(decode_time: u64) -> FragmentHeader {
        FragmentHeader {
            sequence: 1,
            decode_time,
            wall: WALL,
        }
    }

    /// A fragment message as the daemon builds one
    /// (`encode_live_fragment_into`, `crates/iris-api/src/live.rs`): the type
    /// byte, the encoder's prefix of `prft`, `moof`, and the `mdat` header,
    /// the samples, and the trailer.
    fn fragment_message(decode_time: u64, samples: &[Sample]) -> Vec<u8> {
        let encoded = encode_fragment(&header(decode_time), samples).expect("the fixture encodes");
        let mut message = vec![MSG_FRAGMENT];
        message.extend_from_slice(&encoded.prefix);
        for sample in samples {
            message.extend_from_slice(&sample.data);
        }
        message.extend_from_slice(&encoded.trailer);
        message
    }

    /// A video frame of `PAYLOAD_LEN` bytes of `PAYLOAD`, 3000 ticks long,
    /// as the daemon's live frames are.
    fn video(is_sync: bool) -> Sample {
        Sample {
            data: vec![PAYLOAD; PAYLOAD_LEN].into(),
            duration: 3000,
            is_sync,
        }
    }

    /// The message of one sync frame at `decode_time`.
    fn sync_frame(decode_time: u64) -> Vec<u8> {
        fragment_message(decode_time, &[video(/* is_sync */ true)])
    }

    /// The message of one frame at `decode_time` that is not a sync frame.
    fn delta_frame(decode_time: u64) -> Vec<u8> {
        fragment_message(decode_time, &[video(/* is_sync */ false)])
    }

    /// A decoder that has read the init segment of a 1920×1080 HEVC stream.
    fn configured() -> LiveDecoder {
        let mut decoder = LiveDecoder::new();
        assert_hevc_config(
            decoder.decode(&init_message(&hevc_entry(1920, 1080))),
            1920,
            1080,
        );
        decoder
    }

    /// The sample `decoder` gives for `message`, which it must accept; a
    /// refusal fails the test at the line that asked for the sample.
    #[track_caller]
    fn sample_of(decoder: &mut LiveDecoder, message: &[u8]) -> LiveSample {
        match decoder.decode(message) {
            Ok(LiveItem::Sample(sample)) => sample,
            other => panic!("no sample: {other:?}"),
        }
    }

    /// The bytes of `message` that `sample` names.
    fn bytes_of<'a>(message: &'a [u8], sample: &LiveSample) -> &'a [u8] {
        let start = usize::try_from(sample.offset()).unwrap();
        let end = start + usize::try_from(sample.len()).unwrap();
        &message[start..end]
    }

    /// A decoder that has read the init segment of a 1920×1080 HEVC stream
    /// and a sync frame after it, so that it takes every frame.
    fn decoding() -> LiveDecoder {
        let mut decoder = configured();
        sample_of(&mut decoder, &sync_frame(0));
        decoder
    }

    #[test]
    fn an_init_message_gives_the_configuration() {
        let mut decoder = LiveDecoder::new();
        assert_hevc_config(
            decoder.decode(&init_message(&hevc_entry(1920, 1080))),
            1920,
            1080,
        );
    }

    /// `parse_fragment` counts the payload's offset from the fragment's first
    /// byte, its `prft`: the sample starts past the type byte and the
    /// encoder's prefix of `prft`, `moof`, and the `mdat` header.
    #[test]
    fn a_sync_fragment_gives_one_sample_at_its_place_in_the_message() {
        let message = sync_frame(DECODE_TIME);
        let sample = sample_of(&mut configured(), &message);
        let prefix = encode_fragment(&header(DECODE_TIME), &[video(/* is_sync */ true)])
            .unwrap()
            .prefix
            .len();
        assert_eq!(sample.offset(), u32::try_from(1 + prefix).unwrap());
        assert_eq!(bytes_of(&message, &sample), [PAYLOAD; PAYLOAD_LEN]);
    }

    /// The sample of the fixture sync frame at `DECODE_TIME`.
    fn timed_sample() -> LiveSample {
        sample_of(&mut configured(), &sync_frame(DECODE_TIME))
    }

    /// A `tfdt` past 32 bits, which only its version 1 holds.
    #[test]
    fn a_sample_carries_its_fragments_decode_time() {
        assert_eq!(timed_sample().decode_time(), MediaTicks::new(9_000_003_000));
    }

    #[test]
    fn a_sample_carries_its_fragments_trun_duration() {
        assert_eq!(timed_sample().duration(), MediaTicks::new(3000));
    }

    #[test]
    fn a_sample_carries_its_fragments_prft_wall_time() {
        assert_eq!(timed_sample().wall(), Some(WALL));
    }

    #[test]
    fn a_fragment_before_any_init_is_refused() {
        let mut decoder = LiveDecoder::new();
        assert_eq!(decoder.decode(&sync_frame(0)), Err(LiveError::NoConfig));
        assert_eq!(decoder.decode(&delta_frame(3000)), Err(LiveError::NoConfig));
    }

    /// Bytes that do not parse as a fragment are refused as `NoConfig`, not as
    /// the `Fragment` that a decoder which takes every frame gives them
    /// (`an_unparsable_fragment_while_decoding_is_refused_as_malformed`).
    #[test]
    fn a_fragment_without_a_configuration_is_refused_before_it_is_read() {
        assert_eq!(
            LiveDecoder::new().decode(&[MSG_FRAGMENT, 1, 2, 3]),
            Err(LiveError::NoConfig)
        );
    }

    #[test]
    fn a_decoder_made_by_default_is_the_one_new_makes() {
        assert_eq!(LiveDecoder::default(), LiveDecoder::new());
    }

    /// The decoder awaits a sync sample until one comes: the frame right
    /// after the init segment, and the one after it.
    #[test]
    fn a_non_sync_fragment_right_after_an_init_is_refused() {
        let mut decoder = configured();
        assert_eq!(
            decoder.decode(&delta_frame(0)),
            Err(LiveError::AwaitingSync)
        );
        assert_eq!(
            decoder.decode(&delta_frame(3000)),
            Err(LiveError::AwaitingSync)
        );
    }

    #[test]
    fn the_frames_from_a_sync_frame_on_are_taken() {
        let mut decoder = configured();
        assert!(sample_of(&mut decoder, &sync_frame(0)).is_sync());
        assert!(!sample_of(&mut decoder, &delta_frame(3000)).is_sync());
    }

    /// The init segment of a stream of other dimensions, read while the
    /// decoder takes every frame.
    fn second_init() -> Vec<u8> {
        init_message(&hevc_entry(3840, 2160))
    }

    #[test]
    fn a_second_init_gives_the_configuration_of_its_stream() {
        assert_hevc_config(decoding().decode(&second_init()), 3840, 2160);
    }

    #[test]
    fn a_non_sync_fragment_right_after_a_second_init_is_refused() {
        let mut decoder = decoding();
        let _ = decoder.decode(&second_init());
        assert_eq!(
            decoder.decode(&delta_frame(6000)),
            Err(LiveError::AwaitingSync)
        );
    }

    #[test]
    fn a_sync_frame_after_a_second_init_is_taken() {
        let mut decoder = decoding();
        let _ = decoder.decode(&second_init());
        assert!(sample_of(&mut decoder, &sync_frame(9000)).is_sync());
    }

    #[test]
    fn a_fragment_of_two_samples_is_refused() {
        let mut decoder = configured();
        let two = fragment_message(0, &[video(/* is_sync */ true), video(/* is_sync */ false)]);
        assert_eq!(decoder.decode(&two), Err(LiveError::SampleCount(2)));
    }

    #[test]
    fn an_aac_init_message_gives_its_configuration() {
        match LiveDecoder::new().decode(&init_message(&aac_entry())) {
            Ok(LiveItem::Config(DecoderConfig::Aac(config))) => {
                assert_eq!(config.audio_specific_config(), [0x11, 0x08]);
                assert_eq!((config.sample_rate(), config.channels()), (64_000, 1));
            }
            other => panic!("no AAC configuration: {other:?}"),
        }
    }

    /// The bytes of the fixture audio frame.
    const AUDIO_LEN: usize = 200;

    /// The message of one AAC frame of 1,024 samples at 64 kHz, 1,440 ticks.
    fn audio_frame() -> Vec<u8> {
        let frame = Sample {
            data: vec![PAYLOAD; AUDIO_LEN].into(),
            duration: 1440,
            is_sync: true,
        };
        fragment_message(DECODE_TIME, &[frame])
    }

    /// A decoder that has read the init segment of `aac_entry`.
    fn configured_for_audio() -> LiveDecoder {
        let mut decoder = LiveDecoder::new();
        let _ = decoder.decode(&init_message(&aac_entry()));
        decoder
    }

    /// The daemon marks every audio frame a sync sample.
    #[test]
    fn an_audio_fragment_is_a_sync_sample() {
        assert!(sample_of(&mut configured_for_audio(), &audio_frame()).is_sync());
    }

    #[test]
    fn an_audio_sample_lasts_its_trun_duration() {
        let sample = sample_of(&mut configured_for_audio(), &audio_frame());
        assert_eq!(sample.duration(), MediaTicks::new(1440));
    }

    #[test]
    fn an_audio_sample_ends_where_the_trailer_starts() {
        let message = audio_frame();
        let sample = sample_of(&mut configured_for_audio(), &message);
        let start = message.len() - TRAILER_LEN - AUDIO_LEN;
        assert_eq!(
            (
                usize::try_from(sample.offset()).unwrap(),
                usize::try_from(sample.len()).unwrap()
            ),
            (start, AUDIO_LEN)
        );
    }

    #[test]
    fn reset_forgets_the_configuration() {
        let mut decoder = decoding();
        decoder.reset();
        assert_eq!(decoder.decode(&sync_frame(3000)), Err(LiveError::NoConfig));
    }

    #[test]
    fn a_reset_decoder_is_the_one_new_makes() {
        let mut decoder = decoding();
        decoder.reset();
        assert_eq!(decoder, LiveDecoder::new());
    }

    #[test]
    fn an_init_after_a_reset_configures_the_decoder() {
        let mut decoder = decoding();
        decoder.reset();
        assert_hevc_config(
            decoder.decode(&init_message(&hevc_entry(1920, 1080))),
            1920,
            1080,
        );
    }

    #[test]
    fn a_reset_decoder_refuses_a_non_sync_fragment_after_its_init() {
        let mut decoder = decoding();
        decoder.reset();
        let _ = decoder.decode(&init_message(&hevc_entry(1920, 1080)));
        assert_eq!(
            decoder.decode(&delta_frame(6000)),
            Err(LiveError::AwaitingSync)
        );
    }

    /// The init segment of an `avc1` stream, which the decoder does not read.
    fn avc_init() -> Vec<u8> {
        init_message(&visual_entry(*b"avc1", 1920, 1080))
    }

    #[test]
    fn an_init_of_an_unsupported_sample_entry_is_refused() {
        assert_eq!(
            decoding().decode(&avc_init()),
            Err(LiveError::Config(DecoderConfigError::Unsupported(*b"avc1")))
        );
    }

    /// An init message of the type byte alone holds no sample entry.
    #[test]
    fn an_init_without_a_sample_entry_is_refused() {
        assert_eq!(
            decoding().decode(&[MSG_INIT]),
            Err(LiveError::Config(DecoderConfigError::NoSampleEntry))
        );
    }

    /// The fragments after an init segment the decoder could not read
    /// belong to that segment's stream, which no configuration it holds
    /// describes: an `avc1` segment's, and those after an init message
    /// without a sample entry.
    #[test]
    fn a_refused_init_forgets_the_configuration() {
        let avc = avc_init();
        for init in [avc.as_slice(), &[MSG_INIT]] {
            let mut decoder = decoding();
            let _ = decoder.decode(init);
            assert_eq!(decoder.decode(&sync_frame(3000)), Err(LiveError::NoConfig));
        }
    }

    /// The frames after a fragment the decoder refused may depend on it, so
    /// it awaits a sync sample again: after a fragment of two samples, and
    /// after one that does not parse.
    #[test]
    fn a_refused_fragment_waits_for_a_sync_sample() {
        let two = fragment_message(
            3000,
            &[video(/* is_sync */ false), video(/* is_sync */ false)],
        );
        for refused in [two.as_slice(), &[MSG_FRAGMENT, 1, 2, 3]] {
            let mut decoder = decoding();
            let _ = decoder.decode(refused);
            assert_eq!(
                decoder.decode(&delta_frame(9000)),
                Err(LiveError::AwaitingSync)
            );
        }
    }

    /// A sync frame ends the wait that a refused fragment starts.
    #[test]
    fn a_sync_frame_after_a_refused_fragment_is_taken() {
        let mut decoder = decoding();
        let _ = decoder.decode(&[MSG_FRAGMENT, 1, 2, 3]);
        let sample = sample_of(&mut decoder, &sync_frame(12_000));
        assert!(sample.is_sync(), "{sample:?}");
    }

    /// These bytes, which a decoder without a configuration refuses as
    /// `NoConfig`, are refused as `Fragment` while the decoder takes every
    /// frame.
    #[test]
    fn an_unparsable_fragment_while_decoding_is_refused_as_malformed() {
        assert_eq!(
            decoding().decode(&[MSG_FRAGMENT, 1, 2, 3]),
            Err(LiveError::Fragment)
        );
    }

    /// The message of a fragment at `decode_time` whose one sample holds no
    /// byte.
    fn empty_frame(decode_time: u64, is_sync: bool) -> Vec<u8> {
        let empty = Sample {
            data: Vec::new().into(),
            duration: 3000,
            is_sync,
        };
        fragment_message(decode_time, &[empty])
    }

    /// A sample of no bytes holds no picture and no sound; whether a block
    /// buffer takes it is unverified. Its fragment is refused: a sync
    /// frame's while the decoder awaits one, and another frame's while it
    /// decodes.
    #[test]
    fn a_fragment_whose_sample_is_empty_is_refused() {
        assert_eq!(
            configured().decode(&empty_frame(0, /* is_sync */ true)),
            Err(LiveError::Fragment)
        );
        assert_eq!(
            decoding().decode(&empty_frame(3000, /* is_sync */ false)),
            Err(LiveError::Fragment)
        );
    }

    /// The decoder checks a sample before it checks for sync, so an empty
    /// frame that is not a sync frame, while the decoder awaits one, is
    /// refused as malformed and not as `AwaitingSync`.
    #[test]
    fn an_empty_non_sync_sample_while_awaiting_sync_is_refused_as_malformed() {
        assert_eq!(
            configured().decode(&empty_frame(0, /* is_sync */ false)),
            Err(LiveError::Fragment)
        );
    }

    #[test]
    fn an_empty_sync_sample_does_not_end_the_wait_for_one() {
        let mut decoder = configured();
        // Refused, as `a_fragment_whose_sample_is_empty_is_refused` checks.
        let _ = decoder.decode(&empty_frame(0, /* is_sync */ true));
        assert_eq!(
            decoder.decode(&delta_frame(3000)),
            Err(LiveError::AwaitingSync)
        );
    }

    /// An empty frame among decoded ones starts the wait for a sync sample
    /// again, as any refused fragment does.
    #[test]
    fn an_empty_sample_while_decoding_starts_the_wait_for_a_sync_sample() {
        let mut decoder = decoding();
        // Refused, as `a_fragment_whose_sample_is_empty_is_refused` checks.
        let _ = decoder.decode(&empty_frame(3000, /* is_sync */ false));
        assert_eq!(
            decoder.decode(&delta_frame(6000)),
            Err(LiveError::AwaitingSync)
        );
    }

    /// The shortest sample a fragment gives is one byte.
    #[test]
    fn a_sample_of_one_byte_is_placed() {
        let one = Sample {
            data: vec![PAYLOAD].into(),
            duration: 3000,
            is_sync: true,
        };
        let message = fragment_message(0, &[one]);
        assert_eq!(
            bytes_of(&message, &sample_of(&mut configured(), &message)),
            [PAYLOAD]
        );
    }

    #[test]
    fn a_fragment_cut_short_is_refused() {
        let message = sync_frame(0);
        for cut in 1..(message.len()) {
            assert_eq!(
                configured().decode(&message[..cut]),
                Err(LiveError::Fragment),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_fragment_with_a_byte_after_its_trailer_is_refused() {
        let mut longer = sync_frame(0);
        longer.push(0);
        assert_eq!(configured().decode(&longer), Err(LiveError::Fragment));
    }

    #[test]
    fn a_decode_time_of_i64_max_ticks_is_kept() {
        let last = u64::try_from(i64::MAX).unwrap();
        assert_eq!(
            sample_of(&mut configured(), &sync_frame(last)).decode_time(),
            MediaTicks::new(i64::MAX)
        );
    }

    #[test]
    fn a_decode_time_past_the_range_of_media_ticks_is_refused() {
        let last = u64::try_from(i64::MAX).unwrap();
        for past in [last + 1, u64::MAX] {
            assert!(
                matches!(
                    configured().decode(&sync_frame(past)),
                    Err(LiveError::DecodeTime(_))
                ),
                "{past}"
            );
        }
    }

    /// The reader's bounds keep every fragment it accepts inside its
    /// message. The decoder checks the placement again, whatever the reader
    /// gives.
    #[test]
    fn a_sample_is_placed_only_inside_the_message() {
        let message = [0; 10];
        // Four bytes from offset 6, past the type byte, end at the message's end.
        assert_eq!(sample_offset(&message, 5, 4), Some(6));
        assert_eq!(sample_offset(&message, 5, 5), None);
        // A sample in the message's last byte lies inside it, and one after it
        // does not.
        assert_eq!(sample_offset(&message, 8, 1), Some(9));
        assert_eq!(sample_offset(&message, 9, 1), None);
    }

    /// An offset or an end past `u32::MAX`, which the reader's bounds never
    /// give.
    #[test]
    fn a_sample_is_placed_only_inside_u32() {
        let message = [0; 10];
        let max = u64::from(u32::MAX);
        assert_eq!(sample_offset(&message, max, 1), None);
        assert_eq!(sample_offset(&message, max + 1, 1), None);
        assert_eq!(sample_offset(&message, 0, u32::MAX), None);
    }

    /// A message that is neither an init segment nor a fragment is refused
    /// as `live_message` refuses it.
    #[test]
    fn a_message_of_neither_kind_is_refused_by_its_framing() {
        let mut decoder = decoding();
        assert_eq!(decoder.decode(&[]), Err(LiveError::Empty));
        assert_eq!(
            decoder.decode(&[0x03, MSG_FRAGMENT]),
            Err(LiveError::UnknownType(0x03))
        );
    }

    /// A decoder that takes every frame is unchanged by messages the framing
    /// refuses.
    #[test]
    fn a_message_of_neither_kind_leaves_the_decoder_as_it_was() {
        let mut decoder = decoding();
        let _ = decoder.decode(&[]);
        let _ = decoder.decode(&[0x03, MSG_FRAGMENT]);
        assert_eq!(decoder, decoding());
    }
}
