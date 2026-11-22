# 15 — Apple clients

A planned viewer for iOS and macOS: a SwiftUI front end, Apple's media frameworks for decode and
display, and the Rust library crate `iris-client`, which the Linux viewer uses. The tree holds no
Swift code. This document is the research that `AGENTS.md` rule 11 asks for. §4 lists each decision
with the check that settles it. §9 lists what the checks found in the daemon and in other documents.
§10 lists what stays **unverified**, which includes everything on an iOS device. §11 records what is
built in Rust and what that code was checked against.

## 1. What the standards and the platform require

Local text is under `docs/standards/ietf/`, and clause numbers are those of that text. ITU-T H.265
is cited by the clauses of edition V11 (01/2026), read from ITU's free download. The tree holds no
copy of ITU-T H.265 or of ISO/IEC 14496; `docs/standards/README.md` records what stands in for each.
Apple's interface text is quoted from the macOS 27.0 and iOS 27.0 SDKs of Xcode 27.0 (27A5209h).
Most quotations come from the SDK headers, which carry the documentation comments. Swift alone has
the receivers that a render synchronizer gives and the sample buffers they take. Those are quoted
from the Swift interfaces of AVFoundation and CoreMedia in each SDK (`AVFoundation.swiftmodule`,
`CoreMedia.swiftmodule`). Each header cited below is the same file in both SDKs, and both Swift
interfaces declare the receivers and those buffers alike. The SDK files are not in the tree; a
reader checks a quotation against the SDKs of that Xcode build.

| Topic | Source | What it says | Consequence |
|---|---|---|---|
| Fragment timing | draft-pantos-hls-rfc8216bis-22 §3.1.2 | "every Track Fragment Box MUST contain a Track Fragment Decode Time Box ('tfdt'). fMP4 Segments MUST use movie-fragment-relative addressing." | A client parser can rely on `tfdt` and on offsets relative to the `moof`, as the reader in `crates/iris-mp4/src/reader.rs` does |
| Box syntax | ISO/IEC 14496-12 (`tfhd`, `tfdt`, `trun`, `prft`) | Not read; the tree holds no copy (`docs/standards/README.md`). MPEG's conformance material (Sources) publishes the box syntax and not the meaning of the flags | `LiveDecoder` reads a fragment with the daemon's reader, `iris_mp4::reader::parse_fragment` (`crates/iris-client/src/live/decoder.rs`). `iris_client::live::media_time` reads the 32-byte version-1 `prft` itself (`crates/iris-client/src/live/media_time.rs`). How the reader treats the flags is **unverified** against the standard's text |
| Decoder configuration | ISO/IEC 14496-15 (`hvcC`), ISO/IEC 14496-14 (`esds`), ISO/IEC 14496-1 (the ES_Descriptor, DecoderConfigDescriptor, and DecoderSpecificInfo inside `esds`, and their lengths of one to four bytes), ISO/IEC 14496-3 (AudioSpecificConfig and its sampling frequency indices) | Not read; the tree holds no copy (`docs/standards/README.md`). The readers take the layouts that the vendored retina writes for the daemon (`third_party/retina/src/codec/h265/record.rs`, `third_party/retina/src/codec/aac.rs`) and that ffmpeg writes (§11). That MPEG's reference software publishes the `hvcC` layout is **unverified**: no source is recorded for it | The rule that `hvc1` carries every parameter set in `hvcC` is **unverified** against the standard's text; §3.1 shows a hardware decoder taking the sets of the daemon's `hvcC`. The descriptors that the `esds` reader walks, and the rates it gives the frequency indices, are **unverified** against the standards' text. §3.6 shows ffmpeg decoding the daemon's audio by the configuration the reader gives. §11 shows the reader taking an `esds` that ffmpeg wrote, whose descriptor lengths are four bytes each. The bound on the sets of each type that the `hvcC` reader reads comes from the identifier ranges of ITU-T H.265 (the next row); no clause of ISO/IEC 14496-15 was read for it |
| Parameter-set identifiers | ITU-T H.265 (V11) §7.3.2.1, §7.4.3.2.1, §7.4.3.3.1, §7.4.2.4.2 | `vps_video_parameter_set_id` is u(4); "The value of sps_seq_parameter_set_id shall be in the range of 0 to 15, inclusive."; "The value of pps_pic_parameter_set_id shall be in the range of 0 to 63, inclusive."; a slice segment, a set, or an SEI message refers to a set by its identifier, which activates it | A decoder tells apart at most 16 VPSs, 16 SPSs, and 64 PPSs, the most of each that `iris_mp4::decoder_config::parse_hvcc` reads. The bound is the reader's, drawn from these ranges; no clause of ISO/IEC 14496-15 was read for it |
| Playlist comments | RFC 8216 §4.1 | "All other lines that begin with '#' are comments and SHOULD be ignored." | A player never sees `#IRIS-FRAGMENT`; the client reads the playlist text itself to map position to wall time |
| A playlist's listed URIs | RFC 8216 §6.2.1 | "The server MUST NOT change the Media Playlist file, except to" do one of four things. It may "Append lines to it". It may "Remove Media Segment URIs from the Playlist in the order that they appear". It may increment the value of the `EXT-X-MEDIA-SEQUENCE` or the `EXT-X-DISCONTINUITY-SEQUENCE` tag. It may "Add an EXT-X-ENDLIST tag to the Playlist" | A URI listed in one reload stays the same in the next (§9.1) |
| Low-latency delivery | draft-22 Appendix B.1 | "HTTP-delivered Playlists and Segments are served via HTTP/2 [RFC9113] or HTTP/3 [RFC9114]." | The daemon serves HTTP/1.1, so a player may fall back to ordinary latency (§3.3) |
| HEVC NAL units | RFC 7798 §1.1.4, §3.1.1 | "HEVC uses a two-byte NAL unit header"; its first bit is `forbidden_zero_bit`, "Required to be zero in [HEVC]", and a type "less than 32" marks "a VCL NAL unit"; an IRAP picture is one "for which each VCL NAL unit has nal_unit_type in the range of BLA_W_LP (16) to RSV_IRAP_VCL23 (23), inclusive" | A sample's NAL units are checked against these (§3.6); the parameter sets are the types 32 to 34 that `CMFormatDescription.h` names (below) |
| A message too big | RFC 6455 §7.4.1 | "1009 indicates that an endpoint is terminating the connection because it has received a message that is too big for it to process." | The daemon closes a live feed with 1009 when a sync frame passes the message ceiling (`crates/iris-api/src/live.rs`). A viewer ends that session as `SessionEnd::Oversized`, which `iris_client::live::Reconnect` answers with a pause that doubles from 2 s to 60 s (`crates/iris-client/src/live/reconnect.rs`) |
| Private and shared addresses | RFC 1918 §3; RFC 6598 §1, §4, and §7 | RFC 1918 reserves three blocks "for private internets", written "(10/8 prefix)", "(172.16/12 prefix)", and "(192.168/16 prefix)". RFC 6598 §7 records "an IPv4 /10 for use as Shared Address Space", `100.64/10`. Its §4 designates the range "for Service Provider use with the purpose of facilitating CGN deployment". Its §1 says: "Shared Address Space can only be used in Service Provider networks or on routing equipment that is able to do address translation across router interfaces when the addresses are identical on two different interfaces." | The plain-HTTP policy admits these four ranges beside the loopback, and no other address (§4). A tunnel or an overlay network that numbers its peers from the shared range uses it outside the purpose that RFC 6598 gives it |
| HEVC format description | `CMFormatDescription.h` | "The supported NAL unit types to be included in the format description are 32 (video parameter set), 33 (sequence parameter set), 34 (picture parameter set), 39 (prefix SEI) and 40 (suffix SEI)"; the length size: "Pass 1, 2 or 4." | The three sets of `hvcC` and its length size are all the decoder needs |
| Enqueueing samples | `AVSampleBufferDisplayLayer.h`, `AVQueuedSampleBufferRendering.h`, `AVSampleBufferVideoRenderer.h` | The layer's `enqueueSampleBuffer:`, `flush`, and `status` are deprecated from iOS 18 and macOS 15, each in favor of the same member of `sampleBufferRenderer` ("Use sampleBufferRenderer's enqueueSampleBuffer: instead"). That renderer, `AVSampleBufferVideoRenderer`, is available from iOS 17 and macOS 14. For Swift the renderer's methods are deprecated in turn from iOS 27 and macOS 27, in favor of a receiver obtained from the render synchronizer | At the deployment targets, iOS 27 and macOS 27 (§4), the client enqueues through that receiver |
| A receiver's input | AVFoundation's and CoreMedia's Swift interfaces | `Receiver.enqueue(_:)` and `enqueueImmediately(_:)` take a `CMReadySampleBuffer<CMSampleBuffer.DynamicContent>`. CoreMedia, from iOS 26 and macOS 26, makes one from a `CMSampleBuffer` with `init(unsafeBuffer:)`. It also makes one from a `CMReadySampleBuffer<CMReadOnlyDataBlockBuffer>` with `init(_:)`. That buffer comes from a `CMSampleBuffer` with `init(unsafeWithDataBuffer:)`. It also comes from a `CMReadOnlyDataBlockBuffer` with a format description and sample properties (`init(dataBuffer:formatDescription:sampleProperties:)`). A `CMReadOnlyDataBlockBuffer` comes from a `CMBlockBuffer` (`init(unsafeBlockBuffer:)`), `Data`, or `DispatchData` | A sample reaches a receiver through a `CMSampleBuffer` or a `CMReadOnlyDataBlockBuffer` made over its bytes; whether any of these routes holds a range of the message without copying it is **unverified** (§10) |
| Display timing | `AVSampleBufferDisplayLayer.h` | "The combination of either a non-NULL controlTimebase or an AVSampleBufferRenderSynchronizer with the use of kCMSampleAttachmentKey_DisplayImmediately ... is not recommended." | A view with sound uses a synchronizer and real timestamps; a view without may display on arrival; never the two together |
| One clock for sound and picture | `AVSampleBufferRenderSynchronizer.h` | "By default, this timebase will be driven by the clock of an added AVSampleBufferAudioRenderer. If no AVSampleBufferAudioRenderer has been added, the source clock will be the host time clock" | One synchronizer holds the audio renderer and the video renderer |
| Attaching renderers | `AVSampleBufferRenderSynchronizer.h`, `AVSampleBufferAudioRenderer.h`; AVFoundation's Swift interface | For Swift, from iOS 27 and macOS 27, the synchronizer's `addRenderer:` is deprecated: "Get an AVQueuedSampleBufferRenderingReceiver from an audio or video renderer with sampleBufferReceiver() and add it to the synchronizer with addReceiver(_:) instead"; so are `removeRenderer:atTime:completionHandler:` ("Use removeReceiver(_:at:) instead") and `renderers`. The audio renderer's `status`, `error`, and `flushFromSourceTime:completionHandler:` give way to its receiver's results and `flush(fromSourceTime:)`. Its automatic-flush and output-configuration notifications give way to the results "for .successWithSuggestedFlushReason", and its `enqueueSampleBuffer:` and `flush`, the protocol's, to the receiver's (`AVQueuedSampleBufferRendering.h`). Neither SDK's Swift interface declares `addReceiver(_:)`, a renderer's `sampleBufferReceiver()`, or `AVQueuedSampleBufferRenderingReceiver`. In each interface the synchronizer gives the receiver of the video or the audio renderer with `sampleBufferReceiver(adding:)`. It takes the receiver back with `removeReceiver`, labelled `(_:at:)` for audio and `(receiver:at:)` for video. The audio receiver's `EnqueueResult` has `enqueuedWithSuggestedFlush`, where the messages name `.successWithSuggestedFlushReason` | From 27 each renderer joins the synchronizer through `sampleBufferReceiver(adding:)`, which gives the receiver its samples go through; the route the header names, a renderer's `sampleBufferReceiver()` and the synchronizer's `addReceiver(_:)`, is **unverified**, neither interface declaring it |
| Losing the decoder | `AVSampleBufferVideoRenderer.h`; AVFoundation's Swift interface | For Swift, from iOS 27 and macOS 27, the header deprecates `requiresFlushToResumeDecoding` and `AVSampleBufferVideoRendererRequiresFlushToResumeDecodingDidChangeNotification`: "Use the result of AVSampleBufferVideoRenderer.Receiver enqueue(_:) and enqueueImmediately(_:) for .requiresFlushToResumeDecoding instead"; and `AVSampleBufferVideoRendererDidFailToDecodeNotification` with the same words "for .successWithDecodeFailure instead". The renderer's `status` and `error` give way to the receiver's `Receiver.EnqueueResult` and `Receiver.RenderingEvent`: "EnqueueResult from enqueue(_:) and enqueueImmediately(_:), and RenderingEvent from renderingEventsAfterFinishedEnqueuing", and `flushWithRemovalOfDisplayedImage:completionHandler:` to "the receiver's flush(removingDisplayedImage:)": "A flush resets decoder state. The next frame passed to enqueueSampleBuffer: should be an IDR frame". The Swift interface names the results otherwise: `Receiver.EnqueueResult` has `enqueuedWithDecodeFailures` and `cancelledDueToFlushRequiredToResume`, and `Receiver.RenderingEvent`, the elements of `renderingEventsAfterFinishedEnqueuing`, has `didFailToDecode` and `requiresFlushToResumeDecoding` | From 27 the receiver reports a lost decoder and a failed decode, in the result of each `enqueue(_:)` and in its rendering events; the renderer's property and notifications are deprecated there and not used. Recovery is the receiver's flush and then a sync frame |
| Request headers on media | Apple engineer, developer forums thread 20421 | "`AVURLAssetHTTPHeaderFieldsKey` is not a supported API, so you should not use it ... you can use AVAssetResourceLoader." | The bearer cannot ride on the requests AVPlayer makes |
| Plain HTTP to an address | `NSAllowsLocalNetworking` reference | "In iOS 17, iPadOS 17, and macOS 14, ATS no longer allows connections to IP addresses by default. Add individual IP addresses and classless inter-domain routing (CIDR) ranges in the `NSExceptionDomains` dictionary." | The app declares the address ranges it may reach over plain HTTP |
| Local Network privacy | TN3179 | "A local network is an IP network associated with a broadcast-capable network interface. Such interfaces include Wi-Fi and Ethernet, but not cellular (WWAN) or VPN."; the checks "apply to all networking APIs" | The prompt arises on the daemon's own subnet and not through a tunnel; the app needs `NSLocalNetworkUsageDescription` either way |

## 2. How others do it

- No surveyed recorder documents a native client that feeds fragmented MP4 from a WebSocket
  to a sample-buffer renderer. Frigate, Moonfire NVR, and go2rtc play the same kind of feed
  through Media Source Extensions in a browser; go2rtc ranks its transports WebRTC, then
  MSE, then HLS for latency.
- Home Assistant's iOS app plays HLS through AVPlayer and carries a per-stream token in the
  URL path. Its source records the cost of anything more than a URL credential:
  "AVFoundation does its networking outside `URLSession` delegates", worked around there
  with undocumented asset options.
- Grids use reduced streams: Moonfire's live view is always the sub stream. That a commercial
  recorder's phone client limits itself to six feeds is **unverified**: no source is recorded
  for it. So is the statement that a recorder that composites a grid on the server re-encodes.
  This recorder never re-encodes.
- Of the open-source Swift players read in source (Sources), one displays each frame on arrival
  and plays sound against a separate clock. Another holds video a fixed quarter second against a
  host-clock timebase, because frames "shown on arrival" stuttered. A third drives an audio
  renderer and the video renderer from one `AVSampleBufferRenderSynchronizer`. It recovers
  through the renderer's flush notification, which the SDKs of §1 deprecate for Swift from 27.
  The last, one synchronizer for both renderers, is the shape `13-audio.md` §10.4 needs.
- Two of them build the HEVC format description from the `hvcC` box as it stands, and one
  that rebuilds it from parameter sets hard-codes the length size it had parsed.
- Firefox for iOS takes Rust components through Swift bindings that UniFFI generates, packaged
  as an XCFramework inside a Swift package (Mozilla application-services, Sources). That
  Element X ships Rust the same way is **unverified**: no source is recorded for it.

## 3. What a real decoder and a real build showed (2026-11-15)

The test machine is a Mac with Apple silicon, on macOS 27 with Xcode 27, Swift 6.4, and Rust 1.99.1.
For §3.1 to §3.5 the test daemon was a build of this repository's `irisd` whose commit was not
recorded. It recorded H.265 Main at 3840×2160 and 1920×1080 at 30 frames a second, and AAC-LC at
64 kHz in one channel. The probes reached it over plain HTTP on the daemon's network. Every result
here is from macOS 27; none is from iOS, and none from an earlier macOS.

The probes of §3.1 to §3.5 were throwaway programs written for these runs. They were not kept, so no
reader can run them again. Those of §3.1 to §3.4 were Swift command-line programs compiled with
`swiftc -O` and run from a terminal. The probe of §3.4 also ran as an app bundle that the system
started. The probe of §3.5 was a Rust crate, a Swift package, and an XcodeGen description, built by
one shell script. Its call costs were taken by a release build of a benchmark program on macOS and
of a harness spawned inside the simulator. Each paragraph says what its probe did and measured. The
probes ran on the date in this section's heading, in two sittings some hours apart, on different
streams of the same kinds. A figure given as a range spans every run of both sittings unless it says
otherwise. The figures are dated observations of the test machine, not measurements a reader can
repeat. A decision of §4 that rests on one names a second support or carries the word
**unverified**. §3.6 is a separate sitting, with the date in its heading and its conditions in its
text. Its scratch program is not in the tree, and its text names the ffmpeg commands that the
program ran.

### 3.1 The live feed into the hardware decoder

A `URLSessionWebSocketTask` with the bearer in its handshake read the init segment and the
fragments of `/api/streams/{camera}/{kind}/live`. The probe took the parameter sets from
`hvcC`, built the description with `CMVideoFormatDescriptionCreateFromHEVCParameterSets`,
wrapped each fragment's one sample in a `CMSampleBuffer`, and decoded it in a
`VTDecompressionSession` created with
`kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder`. A frame counts as
decoded when the session returns an image and no error.

| Stream | Sessions | Decoded | Decode time, median | Decode time, p95 | Receipt after the frame's `prft` wall time, steady median |
|---|---:|---|---:|---:|---:|
| 1920×1080, first sitting | 2 | 150 of 150 each | 0.77 ms | 1.80 to 1.81 ms | 13 ms |
| 1920×1080, second sitting | 2 | 150 of 150 each | 0.74 to 0.80 ms | 1.34 to 1.56 ms | 5 ms |
| 3840×2160, first sitting | 1 | 90 of 90 | 1.90 ms | 3.91 ms | 60 ms |
| 3840×2160, second sitting | 2 | 90 of 90 each | 2.80 to 3.10 ms | 6.15 to 6.52 ms | 4 to 12 ms |

- **One session went wrong and is not in the table.** The first 1920×1080 session of the
  first sitting delivered its 150 fragments at 15.7 frames a second, and the receipt lag grew
  to a median of 2.6 s. Its decode time was 4.88 ms at the median and 26.53 ms at p95. Every
  frame decoded. It did not recur in the sessions after it and is unexplained.
- The first fragment of a session is a sync frame. The session then delivers the 1.3 to 1.9 s
  of picture recorded since that frame, faster than it was recorded. The feed is level with the
  camera 0.5 to 1.5 s later.
- Live `trun` durations are always 3000 ticks; a gap shows only as a step in `tfdt`, the
  longest seen 193 ms. A client times a frame by its decode time, never by a running sum.
- A live fragment holds one sample, and when that sample is a sync frame the flag is in
  `tfhd` alone (`crates/iris-mp4/src/fragment.rs`); the daemon's reader reports it as
  `starts_with_sync`.
- Three 1920×1080 streams decoded at once, in three runs of 300 frames a stream: 27.2 to
  29.8 frames a second each, every frame decoded but one, which failed with `-12909`. The
  probe's own process used 5.3 to 5.7 % of one core; the system's decoder service was not
  measured.
- The audio feed's init segment is `mp4a` with an AudioSpecificConfig of two bytes; its 400
  frames decoded through `AVAudioConverter` with no error. Frames arrive in bursts, the
  longest gap 176 ms. One session.

### 3.2 Recorded playback through AVPlayer

The probe fetched the playlist with the bearer, made each reference absolute, and gave the
text to AVPlayer; AVPlayer fetched the media itself with the capabilities the daemon had
written into the URIs. It then fetched the played byte ranges again and matched each
sample's time against the frames an `AVPlayerItemVideoOutput` delivered.

| Way the playlist reaches AVPlayer | 1920×1080 | 3840×2160 |
|---|---|---|
| `AVAssetResourceLoaderDelegate` under a custom scheme | plays, both sittings | plays, first sitting |
| A listener on the loopback address | plays, first sitting | plays, both sittings |
| The unsupported header option, for comparison | plays, first sitting | plays, first sitting |

- A media playlist was ready to play in 0.7 to 1.1 s, with no entry in the error log and no
  stall. A first frame came 0.04 to 0.90 s after a seek.
- **Segments whose byte range spans several fragments play, every sample presented.** A segment held
  `prft moof mdat free` three times over. Every source sample was displayed: 406 to 417 samples in
  each of four runs of the first sitting, 325 and 335 in the second, and 795 across an
  `EXT-X-DISCONTINUITY` with a second `EXT-X-MAP`. On macOS 27 this answers open question 1 of
  `06-clients.md` §6: playlists need not map one fragment to a segment. On iOS it is **unverified**
  (§10).
- A recorded fragment's last sample carries the time to the next fragment, so a longer gap
  there shows as a step of 67 to 167 ms on screen. The step is in the recording.
- The resource loader is asked for each playlist twice, and accepts the content type
  `public.m3u-playlist`.
- The camera's multivariant playlist with its audio rendition played with the audio track enabled in
  three runs, its media relayed through a proxy. It was ready in 1.8 to 1.9 s, and a first frame
  came 0.9 to 2.4 s after a seek through that proxy. Every source sample was displayed, 378 to 388
  in a run. Sound against picture was not heard. AVPlayer opened four to five connections at once
  for it. Fetching its media from the daemon directly was not measured.
- Of the media routes only `/api/segments/{id}/file.mp4` reads `Range`. `/keyframe` and
  `/export.mp4` answer a range request with 200 and the whole body, and `AVURLAsset` given
  such a URL fails (`-11850`). Downloaded to a file, both open: `AVAssetImageGenerator`
  made one image from a keyframe in 30 to 88 ms, and `AVAssetReader` read every frame
  inside the edit of a 20 s export.
- An export with two tracks opens with both, the audio at its 90 kHz timescale.
  AVFoundation takes 2112 samples (33 ms) off the audio's start as encoder priming although
  the edit list's media time is 0. One file.

### 3.3 The low-latency playlist

- Over plain HTTP/1.1 AVPlayer plays it and logs `-15410`, "Low Latency: Server must
  support http2 ECN and SACK". How far behind it then runs varied: 10.9 s and 11.7 s in the
  first sitting, at a 9 s offset from live; 4.4 s in the second, with one stall and two
  `-16832` warnings.
- Given through a resource loader under a custom scheme, the same playlist ran at its 1.5 s
  part hold-back, 1.75 to 2.3 s behind, with blocking reloads answered in about half a
  second. Three runs, first sitting.
- At a change of minute in each of those runs AVPlayer logged `-12312` and stalled. Two then
  ran 2.7 s and 3.0 s further behind; the third ended 1.1 s further behind (§9.1).
- The audio playlist is not served low-latency (`13-audio.md` §10.5).

### 3.4 Transport security and Local Network privacy

- Run from a terminal, a program reached the daemon over plain HTTP by address through
  `URLSession`, `URLSessionWebSocketTask`, and AVPlayer whatever its `Info.plist` held. TN3179
  exempts command-line tools run from a terminal, so this shows nothing about an app.
- The same program, as an app bundle started by the system with no Local Network permission,
  failed every request with `NSURLErrorDomain -1009`, "The Internet connection appears to be
  offline". The system log read "Local network prohibited". That is the code of any offline
  failure. One run.
- A block by transport security, and an app that holds the permission, were not observed.

### 3.5 A Rust library under a Swift app

A throwaway crate depended on `iris-core` and `iris-mp4` by path, copied the workspace's lint tables
and release profile as they stand, and exported the fragment reader through UniFFI 0.32.2 in
proc-macro mode. One script built it for five targets, generated the Swift, assembled an XCFramework
and a Swift package, and built an app from an XcodeGen description. Run twice from an empty build
directory, it finished in 129 s and 145 s.

| Product | Built | Ran |
|---|---|---|
| macOS app, arm64 and x86-64 | yes | On arm64: Rust tests, Swift package tests, and app tests pass |
| iOS simulator app, arm64 and x86-64 | yes | On arm64, iOS 26: the same checks pass as a program spawned inside the simulator; `xcodebuild test` never started its test process on this Xcode |
| iOS device app, arm64, unsigned | yes, links | not run |

- `iris-core`, `iris-h265`, `iris-mp4`, and `iris-tls` compile with no change to their source for
  `aarch64-apple-ios`, `aarch64-apple-ios-sim`, and `aarch64-apple-darwin`.
- **`unsafe_code = "forbid"` passes and does not read the generated code.** The expansion
  holds 121 `#[unsafe(no_mangle)]` items, 69 `unsafe impl`, and 11 `unsafe` blocks, and no
  lint reports them. A hand-written `unsafe` block and a hand-written `#[unsafe(no_mangle)]`
  in the same crate are both refused. Clippy with warnings as errors and rustdoc pass.
- With `crate-type = ["lib", "staticlib"]` cargo applies no link-time optimisation: the
  release archive was 29.5 MB. Built as `cargo rustc --crate-type staticlib` it is 6.4 MB.
  The unstripped iOS executable of the test app is 918 KB.
- Cargo does not rebuild when a deployment target changes; linking objects built for a
  later OS than the app's draws a warning for each. A target directory keyed by the
  deployment targets avoids the mix.
- The generated Swift compiles in Swift 6 mode with complete concurrency checking and no warning,
  and fails with four errors under a default of main-actor isolation, which an app target may set.
  It belongs in a separate package target.
- A second copy of the generated Swift in one process, as a test bundle hosted in an app
  would carry, re-registers the callback table on its first call, and the first copy's next
  callback aborts. Shown with a program arranged that way; a hosted test bundle itself did
  not run.
- A record with a secret in it would print it: the generated Swift struct prints every
  field.
- Three things were seen once, and their output was not kept. An app built from a changed Rust
  signature and stale generated Swift passes the link step. It then stops at start with "UniFFI API
  checksum mismatch". The app builds with no Rust toolchain on the path once the XCFramework and the
  generated Swift exist. Under the `panic = "abort"` of the release profile, a panic in Rust ends
  the process.

Cost of one call from Swift, release build, mean of 10,000:

| Call | macOS | iOS simulator |
|---|---:|---:|
| Parse a fragment of 4 KiB passed as `Vec<u8>` | 4.2 to 4.6 µs | 8.2 to 17.0 µs |
| Parse a fragment of 200 KiB passed as `Vec<u8>` | 103 to 143 µs | 114 to 201 µs |
| The same fragments passed as `&[u8]` | 1.6 to 3.1 µs | 1.3 to 2.4 µs |
| The parse alone, inside Rust | 36 to 49 ns | 44 to 72 ns |
| Returning a record of four integers | 0.7 to 1.5 µs | 0.6 to 0.7 µs |

`Vec<u8>` is copied three times on the way in; `&[u8]` is not copied, and Rust reads at the
address Swift holds. Bytes from Rust to Swift are always copied. Cancelling a Swift `Task`
does not stop a Rust `async fn` it awaits.

### 3.6 The live decoder of `iris-client` on the daemon's feeds (2026-11-16)

Each live feed of the test daemon was read for 12 s over the network on the same Mac. The feeds
were the H.265 main and sub streams at a nominal 30 frames a second, and AAC-LC audio at 64 kHz in
one channel.

A scratch program, not in the tree, opened `/api/streams/{camera}/{kind}/live` with the bearer in
the handshake. It handed each binary message, type byte included, to one
`iris_client::live::LiveDecoder` for each feed. For each sample it took the bytes of the message
that the sample names: `len()` bytes from `offset()`. It made these checks:

- A video sample had to walk as NAL units behind four-byte lengths that cover it exactly. Each NAL
  unit had to hold at least the two bytes of its header, with its `forbidden_zero_bit` clear (RFC
  7798 §1.1.4 in §1).
- The first VCL NAL unit of a video sample, of a type below 32, had to be of an IRAP picture, a type
  from 16 to 21, exactly when `is_sync()` was set. RFC 7798 §3.1.1 puts the IRAP types at 16 to 23,
  the last of them `RSV_IRAP_VCL23`.
- The parameter sets (types 32 to 34) inside samples were counted.
- `decode_time()` had to rise from one sample to the next, and the steps larger than the sample's
  duration were counted.

The program then wrote each feed to a file. For video it wrote the parameter sets of the
configuration and every NAL unit as an Annex B stream. For audio it wrote each sample behind an ADTS
header built from the AudioSpecificConfig. ffmpeg 9.0.2 decoded each file. `ffprobe -count_frames`
gave the frame count and the picture size, or the rate and the channels.
`ffmpeg -v error -i FILE -f null -` gave the decode errors.

- Each feed gave exactly one `LiveItem::Config`, from its init segment, and then a sample from
  every fragment after it. There was no refusal of any kind: no `AwaitingSync`, since each
  session's first fragment is a sync frame (as in §3.1), and no other `LiveError`.
- Every sample lay inside its message, and every video sample walked exactly as length-prefixed
  NAL units, its sync flag matching an IRAP picture. No sample carried a parameter set, as
  `hvc1` has it.
- ffmpeg decoded every sample the decoder placed: on every feed the frames decoded equal the
  samples given, with no decode error. The picture size ffmpeg read from the parameter sets
  equals the configuration's width and height on every video feed, and every audio feed decoded
  at 64,000 Hz in one channel, as its configuration says.
- Video `trun` durations were always 3000 ticks. On video feeds `decode_time` stepped past the
  duration at times, the longest step 12,600 ticks (140 ms) more than the duration: a gap shows only
  in `tfdt`, as §3.1 found. Audio durations were always 1440 ticks (1024 samples at 64 kHz), with no
  such step in 12 s.
- Every sample carried a `prft` wall time. On audio feeds it never fell. On video feeds it fell
  between consecutive frames at times, never by more than 1 µs and never on a sync frame. It rose
  by up to about 220 ms at a time across stalls.

The fall comes from two clock readings in the daemon (§9.3). A viewer orders and times frames by
`decode_time()`, as §3.1 says, and treats the wall time as a label.

## 4. Decisions and what settles each

| Decision | Basis | Check |
|---|---|---|
| The interface and all decode and display are Swift, in one SwiftUI target for iOS and macOS | Hardware HEVC is reached through AVFoundation and VideoToolbox; from Rust every such call is `unsafe` | §3.5 built a macOS app, an iOS simulator app, and an unsigned iOS device app from one XcodeGen description, each over a Rust library. §3.5 does not record that the apps were SwiftUI, or that one target served both platforms: both are **unverified**. Display on either platform is **unverified** |
| The rules a viewer applies are in the Rust library crate `iris-client`. The Linux viewer uses it, and the planned Apple viewer takes it under UniFFI | One implementation of answer validation, playlist rewriting, the timeline, and fragment parsing; rule 8's fuzz targets come with it | §3.5 put `iris-core` and `iris-mp4` under a Swift app through UniFFI. The Linux viewer uses every part of `iris-client` but the live decoder (§5), with its transport in `iris-transport` (`crates/iris-gui/Cargo.toml`). `iris-client` under UniFFI and in a Swift app is **unverified** |
| A binding crate `iris-ffi` is the one place generated `unsafe` lives, and holds no hand-written `unsafe` | The code that UniFFI generates contains `unsafe` (§3.5). `STANDARDS.md` §6 says: "`unsafe` is forbidden. If a need appears, it goes in its own crate with a `SAFETY:` comment and a benchmark that justifies it." | §3.5: `forbid` refuses hand-written `unsafe` in such a crate and does not read the generated code, a dated observation. How generated code meets the rest of §6, the `SAFETY:` comment and the benchmark, is undecided: **unverified**. §6 names one need for `unsafe`, in the vendored retina; `iris-ffi` would be a second, which §6 does not name. The tree holds no such crate |
| `iris-client` does no input or output: it builds requests and checks answers, and Swift's `URLSession` carries them | The media is fetched by AVPlayer whatever the library does; a background `URLSession` finishes a download after the app is suspended; the bearer stays out of Rust; the crate keeps no C dependency | §3.1 read the live feed and §3.2 the playlists through `URLSession`; `cargo tree` lists no networking, TLS, or C crate under `iris-client` (§11); background downloads **unverified** |
| The Linux viewer's transport is Rust, in the crate `iris-transport`, which depends on `iris-client` | The Linux viewer has no `URLSession`. The request builders and the answer checks stay in `iris-client`, where a Swift transport would use the same ones, and `iris-client` keeps one concern, the rules (`STANDARDS.md` §4) | On 2026-11-18 `cargo tree --locked --workspace -i iris-transport` listed `iris-gui` alone. The transport against a daemon is **unverified** in this document: its tests run against fixture servers and loopback WebSockets in the same crate (§11) |
| A plain-HTTP origin is accepted only for a host on the loopback or at an address in a private or shared range. The app says that the bearer and the video travel unencrypted there. An `https` origin is verified by the system's trust | The Linux viewer refuses plain HTTP off the loopback without `--allow-insecure-http`; the planned Apple viewer is for the daemon's network or a tunnel to it | RFC 1918 §3 and RFC 6598 §7 give the ranges (§1), and `PlainHttp::PrivateAddresses` admits them (`crates/iris-client/src/origin.rs`). That a tunnel or an overlay network numbers its peers from `100.64/10` is **unverified**: no source is recorded for it. RFC 6598 §1 says of that range: "Shared Address Space can only be used in Service Provider networks or on routing equipment that is able to do address translation across router interfaces when the addresses are identical on two different interfaces." By that text an address in the range can be a host in a service provider's network, and the policy would send it the bearer and the video unencrypted. The app's statement and the system's trust are **unverified** |
| A reverse proxy under a private certificate authority is outside the first client | AVPlayer fetches media itself and takes no trust from the app (§2) | **unverified**; it would need the resource loader to carry the media |
| Live picture: WebSocket, `iris_client::live::LiveDecoder` over the daemon's fragment reader, `CMSampleBuffer`, `AVSampleBufferVideoRenderer` | The feed is one sample per fragment; the hardware decoder takes it as it is | §3.1 for decode. §3.6 ran every live feed of the test daemon through `LiveDecoder`, and ffmpeg decoded each sample it placed. The fuzz target `live_sample` runs the decoder on arbitrary bytes. Drawing through the renderer is **unverified** |
| A frame is timed by its `tfdt` decode time | Live durations are constant and gaps show only in `tfdt` | §3.1, and §3.6 through `LiveDecoder`, both dated observations |
| The format description comes from the three parameter sets of `hvcC`, which `iris_mp4::decoder_config` reads and checks; a length size other than four bytes is refused | `CMFormatDescription.h`; the daemon writes four (`third_party/retina/src/codec/h265/record.rs`), and `iris_mp4::decoder_config::parse_hvcc` refuses any other | §3.1 |
| The readers of `hvcC` and of `esds` are in `iris-mp4`. `LiveDecoder` uses both, and the replay server uses the `hvcC` reader | Each reads a decoder configuration, and each reader faces the network (rule 8) | `DecoderConfig::parse` read the test daemon's init segments (§11). ffmpeg decoded the daemon's samples by the parameter sets and the AudioSpecificConfig that it gave (§3.6). It also read an `hev1` and an `mp4a` init segment that ffmpeg wrote (§11). The fuzz target `decoder_config` runs both readers |
| Bytes cross into Rust as `&[u8]`, and a sample comes back as an offset and a length into the message Swift holds | No copy in, and nothing but a small record out | The record is `iris_client::live::LiveSample`, whose `offset()` counts from the first byte of the message (`crates/iris-client/src/live/decoder.rs`). That `&[u8]` crosses without a copy, at 1.3 to 3.1 µs a frame, is a dated observation of §3.5, **unverified** beyond it |
| The view decodes the frames a session opens with and does not show them. It shows the picture from the first frame that arrives no faster than it was recorded, judged by arrival time against media time | §3.1: a session opens with up to 1.9 s of frames since the last sync frame, delivered faster than they were recorded | **unverified** |
| A view without sound shows each frame as it decodes | `13-audio.md` §10.4 holds the picture only once the audio feed delivers; the Linux viewer does the same | **unverified** on screen: receipt is uneven (§3.1), and one player in §2 added a hold for that reason |
| Live sound: the audio feed goes into `AVSampleBufferAudioRenderer` through a receiver. One `AVSampleBufferRenderSynchronizer` gives that receiver and the video renderer's, each with `sampleBufferReceiver(adding:)`. `iris_client::sound_clock` places sound against picture by the rule of `13-audio.md` §10.4 | The SDK's guidance (§1). For Swift from 27 the synchronizer's `addRenderer:` and the audio renderer's `enqueue` and `flush` are deprecated in favor of the receivers (§1). The Linux viewer and the planned Apple viewer place sound by one rule | decode in §3.1. The Linux viewer places its feeds through `iris_client::sound_clock` (`crates/iris-gui/src/sound.rs`). Sound against picture, and both receivers on one synchronizer, are **unverified**. So is the route the header names through `addReceiver(_:)`, which neither SDK's interface declares |
| On a lost decoder, which the receiver's `enqueue` result reports (`cancelledDueToFlushRequiredToResume`, §1), the view flushes the receiver and opens a fresh session | A session opens at a sync frame; waiting in the running one may take a whole group of pictures | **unverified** |
| The grid plays sub streams, one decoder per visible tile; a focused camera plays its main stream | §2; a tile's decoder is released when it leaves the screen | **unverified** beyond §3.1's dated observation of three streams decoding at once on a Mac; so is a device's ceiling |
| Recorded playback: the client fetches playlists with the bearer, `iris-client` checks and rewrites them, and an `AVAssetResourceLoaderDelegate` hands them to AVPlayer | The bearer cannot ride on AVPlayer's requests (§1); the media URIs carry capabilities | §3.2, whose probe made the references absolute itself. `iris_client::playback` gives each playlist body the name a player requests (`crates/iris-client/src/playback.rs`). The rewrite of `iris_client::vod` in AVPlayer is **unverified** |
| Playback of a main stream with recorded audio plays its sound, through a multivariant playlist that `iris_client::vod` builds, as the Linux viewer's playback does | `crates/iris-client/src/vod.rs` builds it and names its two media playlists | §3.2 played the daemon's multivariant playlist through a proxy. The playlist that `iris_client::vod` builds, sound against picture, and direct media are **unverified** in AVPlayer |
| A client plays one recorded window at a time | §3.2: four to five connections for one playlist with audio, against the daemon's bound on streaming media answers shared by every client | **unverified** against that bound |
| The unsupported header option is not used | Apple's statement (§1) | the statement; §3.2 shows the supported way suffices |
| Position maps to wall time through `#IRIS-FRAGMENT`, read by `iris_client::vod` and mapped by `iris_client::timeline_model` | RFC 8216 §4.1; a recorded playlist carries no `EXT-X-PROGRAM-DATE-TIME` | The Linux viewer seeks and draws its playhead through the model (`crates/iris-gui/src/timeline.rs`, `crates/iris-gui/src/playback.rs`). That AVPlayer's position equals the sum of the fragments' ticks is **unverified** |
| A keyframe or an export is downloaded to a file before it is opened | Those routes read no `Range` | §3.2, a dated observation. In the daemon only the route of a recording file reads the `Range` header (`crates/iris-api/src/segments.rs`) |
| The app does not play the low-latency playlist, which is outside this design (§6) | §3.3: 4 to 12 s behind over HTTP/1.1, a stall at each change of minute, and no sound | draft-22 Appendix B.1, whose low-latency delivery is over HTTP/2 or HTTP/3 where the daemon serves HTTP/1.1 (§1), and RFC 8216 §6.2.1, which the renaming at each minute breaks (§9.1); §3.3's figures are dated observations |
| The app declares `NSAllowsLocalNetworking`, the address ranges of RFC 1918 §3 and RFC 6598 §7 as exceptions, and `NSLocalNetworkUsageDescription`; a tunnel numbered outside those ranges is added | §1 | **unverified** in an app, and on iOS. That tunnels and overlay networks number their peers from those ranges is **unverified** too: no source is recorded for it. RFC 6598 §1 keeps the shared range to service provider networks and to equipment that translates addresses (the plain-HTTP row above) |
| The bearer is kept in the Keychain, passed as an argument, and never placed in a UniFFI record | §3.5: a generated Swift struct prints its fields | §3.5 for the hazard; the storage **unverified** |
| The deployment targets are iOS 27 and macOS 27 | The receiver that replaces the renderer's `enqueue` and `flush` for Swift starts there. It reports a lost decoder and a failed decode in its results (§1). So one code path serves both platforms, and nothing that path calls is deprecated there. The renderer's `requiresFlushToResumeDecoding`, its notification, and `AVSampleBufferVideoRendererDidFailToDecodeNotification`, which the SDKs deprecate for Swift from 27, are not used. A build is for the builder's devices (§6) | The SDKs' declarations (§1); §3 ran on macOS 27; iOS 27 **unverified** |
| Samples are enqueued through the receiver a render synchronizer gives for the renderer, each as a `CMReadySampleBuffer<CMSampleBuffer.DynamicContent>` made over the sample's range of the message (§1) | §1 | **unverified**: §3.1 decoded in a `VTDecompressionSession` and enqueued nothing. Whether that buffer keeps the range without a copy is listed in §10 |
| The XCFramework is built by `cargo rustc --crate-type staticlib` for five targets and wrapped in a Swift package; the Xcode project is generated from a description | §3.5 | **unverified** beyond the dated observation of §3.5: its script built the library that way, and the script was not kept |
| Signing identifiers stay in a file git ignores | Rule 9 keeps a team and a bundle identifier out of the tree | **unverified**: §3.5 signed nothing for a device |
| The generated Swift is a separate package target, and Swift tests are not hosted in the app | §3.5: the generated Swift fails to compile under a default of main-actor isolation, and a second copy of it in one process aborts | **unverified** beyond the dated observations of §3.5, the second of them from a program arranged to hold two copies |
| The interface of `iris-client` is synchronous | The crate does no input or output, so the cancellation gap of §3.5 does not arise | `git grep async crates/iris-client` finds nothing, and the crate depends on no async runtime (`crates/iris-client/Cargo.toml`). The gap is a dated observation of §3.5 |
| The Swift tree sits outside `crates/`; its build steps are `iris-tools` subcommands behind one-line `just` recipes | Every directory under `crates/` is a package of the Cargo workspace (`Cargo.toml`), and a Swift tree is not a Cargo package. `AGENTS.md`: "Implement project-owned tools and automation in Rust." | The `doc` recipe is built that way (`crates/iris-tools/src/doc.rs`). The tree holds no Swift file and no build step for one, so the place of the Swift tree is **unverified** |

## 5. The library crate `iris-client`

The Linux viewer is a binary crate, so nothing can depend on its code. The rules a viewer applies
are therefore in the library crate `iris-client` (`crates/iris-client`). The Linux viewer's
transport is the crate `iris-transport` (`crates/iris-transport`), which depends on `iris-client`.
The Linux viewer uses every part below but the decoder configuration and the live decoder. Those two
serve a viewer that decodes the feed itself, which is the design of §4 for the planned Apple viewer.
A rule kept in one crate cannot come to differ between the Linux viewer and the planned Apple
viewer.

| Part | Where | What it gives a viewer | In the Linux viewer |
|---|---|---|---|
| Answer types, limits, and validators | `iris_client::api`: `types.rs`, `limits.rs`, and `validate.rs` under `crates/iris-client/src/api/` | A viewer decodes the daemon's answers into these types and checks them against the bounds of `limits.rs`. A bound has the daemon's value where the daemon has a limit. Each refusal is an `ApiError` whose text a viewer shows | used as they are |
| Request builders and answer checks | `iris_client::api`: `request.rs`, `answer.rs`, and `error.rs` | One table of each call's route, statuses, byte limit, and read idle timeout, without a transport, so `URLSession` and reqwest carry the same requests (§4) | sent and checked by its transport, `iris-transport` |
| The API origin | `iris_client::origin` | The plain-HTTP policy of §4, in the type that every request and live URL is built from. The type refuses a URL with a path prefix | built from the `--api` and `--allow-insecure-http` options; the viewer adds advice to two refusals |
| Playlist validation and rewriting | `iris_client::vod` | A player fetches only the references that the rewrite resolves onto the API origin, and the timeline reads the `#IRIS-FRAGMENT` timing of the same body | used as it is |
| The media time of a live fragment | `iris_client::live::media_time` | The sound placement takes a feed's media time from the `prft` box, in 90 kHz ticks | read from each payload on its way into the demuxer (`crates/iris-gui/src/sound.rs`) |
| The timeline model | `iris_client::timeline_model` | One mapping between wall time and a player's position, and the geometry of a bar. A position is a `PlaylistOffset`, not a toolkit's clock time | drawn by the timeline widget, which keeps its drawing and its labels |
| The playback selection | `iris_client::playback` | The playlist bodies of one fetch, each under the name a player requests, with the thumbnail times and the export name. A resource loader can answer from them | served by the viewer's loopback endpoint |
| Sound placement | `iris_client::sound_clock` | The rule of `13-audio.md` §10.4, as the offsets a viewer sets | set on the GStreamer pads after the demuxers |
| The live protocol, reconnect policy, and byte budget | `iris_client::live`: `protocol.rs`, `reconnect.rs`, and `budget.rs` under `crates/iris-client/src/live/` | The framing of a message, when a feed connects again, the bound on live bytes, and the `LiveSink` trait that a feed pushes into | its source element implements `LiveSink` |
| Decoder configuration | `iris_mp4::decoder_config` (`crates/iris-mp4/src/decoder_config.rs`) | `LiveDecoder` reads `hvcC` and `esds` with it, and the replay server reads `hvcC`. Each reader faces the network (rule 8) | not used |
| A live decoder | `iris_client::live`: `decoder.rs` | A viewer that decodes the feed itself takes an init segment's configuration and a fragment's one sample, as an offset and a length into the message | not used: the viewer pushes its messages into GStreamer's demuxer |

The transport is outside `iris-client` (§4). `iris-transport` sends the requests of `iris-client`
over reqwest and tungstenite, downloads an export, and runs the live feed loop. Each of its failures
is a `thiserror` variant. The Linux viewer shows the text of a failed call or export, and the feed
logs the text of a failed session. A failure that another error caused keeps that error as its
source, or, with `#[error(transparent)]`, has that error's text and source. In this design the
binding crate of §4, `iris-ffi`, wraps `iris-client` and nothing else; the tree holds no such
crate. `iris-client` has no C dependency (`cargo tree`, §11). `just cross-test` runs its tests as
64-bit ARM code with those of the other crates without C dependencies (§11).

Each parser among these parts faces the network and has a fuzz target (rule 8). Five targets depend
on `iris-client`: `vod_playlist` (the playlist rules), `live_fragment` (the media time of a
fragment), `live_message` (the framing), `live_sample` (the live decoder), and `client_answers` (the
answer checks). The target `decoder_config` runs the readers of `iris-mp4`. The tests of
`iris-client` and of `iris-transport` need no toolkit, and they pass on Linux and on a Mac (§11).
The Linux viewer's tests need the GTK 4 and GStreamer development libraries, and its display checks
need a display (`just display-test`). The checks of a Swift tree would need a Mac with Xcode.

The Rust tests cannot check the policy that a Swift transport must apply itself. That policy has
five parts. It refuses redirects. It caps a body as it streams. It applies the read idle timeout. It
admits a WebSocket message of at most `LIVE_MESSAGE_BYTES`, 16 MiB and one byte. It ends a session
as `SessionEnd::Oversized` after a longer message or a 1009 close. RFC 6455 §7.4.1 gives that close
to an endpoint that "has received a message that is too big for it to process". `iris-client` gives
a transport what the policy needs. Each request names its read idle timeout
(`HttpRequest::read_idle`). From the head of an answer, `HttpRequest::body_to_read` says what to
read of the body: nothing, or the daemon's message or the value up to a limit. That gives a
transport a point to stop early. Each answer check takes the final URL, the status, the content
type, and the body that the transport read whole within that limit. Each part of the policy on the
Swift side is **unverified** (§10): no Swift transport exists to test.

## 6. The first client

Three views, on iOS and macOS alike:

- **Live grid.** One tile per camera on its sub stream, decoding only while visible.
- **Focus.** One camera on its main stream, with sound when it has an audio feed.
- **Playback.** A window of one camera through AVPlayer, with a timeline of recorded coverage and
  events drawn from `iris_client::timeline_model`, and the recording's sound.

The first client is complete when each of these holds. Each is measured on an iPhone and on a Mac,
except the grid, which is measured on an iPhone:

- A view without sound shows the camera within the 150 to 300 ms that `08-roadmap.md` sets
  for the daemon's network. A view with sound holds half a second by `13-audio.md` §10.4,
  so it is that much later and its sound is with its picture.
- A grid of six tiles decodes on an iPhone.
- A view recovers its picture after the app returns from the background.
- A seek in playback shows a frame within a second, and the timeline's wall time agrees
  with the recording's.
- Each item of §10 the client depends on is measured, or carries the word **unverified**.

A refusal says what to do. A Local Network denial, a block by transport security, and a
daemon that does not answer must be told apart. §3.4 shows that the first carries the code
of any offline failure. How to tell the three apart is **unverified**.

Export, thumbnails, Picture in Picture, camera provisioning, and an event list follow. Outside this
design altogether: notifications, which nothing the daemon does calls for; AirPlay; the low-latency
playlist; a store, notarization, and the sandbox entitlements a store review asks for. A build is
made by the person who runs it, for that person's devices. Without a paid developer membership, an
iOS build is rebuilt and installed again every seven days, which this design accepts. Apple's
"Developer account overview" (Sources), read on 2026-11-18, says of an account without a membership:
"Provisioning profiles that enable apps to be installed on a device will expire 7 days from
issuance. You’ll need to rebuild and reinstall your app to your device after expiration."

In this design the Swift tree has the XcodeGen description, the Swift package that wraps the
XCFramework, and the app's sources. In this design git ignores its build output, Xcode's user data,
and the signing settings. The CI workflow has no macOS runner, so CI checks no part of a Swift tree.

## 7. License

The workspace is GPL-3.0-or-later. A build a person makes for their own devices raises no
question. Distribution through an app store is a matter its copyright holder decides; this
is no legal advice. UniFFI is MPL-2.0, which `deny.toml` allows.

## 8. Against the roadmap

Phase 4 of `08-roadmap.md` is this design: one app for iOS and macOS over `iris-client`. The three
views of §6 come first. Export sharing, thumbnails, Picture in Picture, provisioning, and an event
list follow. The phase has three exit criteria, each a criterion of §6:

- live latency of 150 to 300 ms on the LAN for a view without sound;
- a grid of six tiles on an iPhone;
- a view's recovery of its picture after the app returns from the background.

§6 has the other criteria of the first client.

## 9. What the checks found beyond this design

### 9.1 The low-latency playlist renames what it has listed

The rolling capability's expiry is rounded to the minute so that reloads within one minute
write the same URIs (`crates/iris-api/src/auth.rs`). Across a minute every URI in the
playlist changes: one reload a minute after another listed the same `init.mp4` and the same
segments under a different expiry and signature. RFC 8216 §6.2.1 lets a server append and
remove, not rewrite, and AVPlayer answered with `-12312` and a stall (§3.3).

### 9.2 Media routes that read no ranges

Only the recording file route reads `Range` (§3.2). The Linux viewer downloads keyframes
and exports whole, so nothing depends on it; a player handed either URL fails.

### 9.3 A frame's wall time can fall by a microsecond

On the video feeds of §3.6 the `prft` wall time fell between consecutive frames at times, never by
more than 1 µs and never on a sync frame. The daemon maps the receipt `Instant` of a frame to wall
time through two clock readings, `Instant::now()` and `WallUs::now()`, taken as the frame is
processed (`WallClock::wall_at` in `crates/iris-ingest/src/session.rs`). The two readings are taken
one after the other and truncated to microseconds. So frames received together, as after a stall,
come out up to a microsecond apart in either order. A viewer orders and times frames by decode time
and treats the wall time as a label (§3.6).

### 9.4 Where the clients appendix differs

`appendix/C-clients.md` (A5, A6) names `kVTDecompressionPropertyKey_RealTime` and recovery on
`kVTInvalidSessionErr`. Those belong to a decompression session that the app creates. Behind a
render synchronizer the receiver reports a lost decoder as `cancelledDueToFlushRequiredToResume` in
the result of an `enqueue` (§1). The receiver then needs a flush. `06-clients.md` §2 says the same.

### 9.5 Questions of other documents that §3 answers

Three questions that other documents ask have a dated answer from §3, on macOS 27 alone. Each of
those documents records the answer, and each is **unverified** on iOS:

- `06-clients.md` §6, question 1: a segment whose byte range spans several fragments plays (§3.2).
- `13-audio.md` §1 and §4: AAC-LC at 64 kHz decodes (§3.1), and AVFoundation opens an audio track at
  the 90 kHz timescale (§3.2).
- `13-audio.md` §9.1: an export with two tracks opens in AVFoundation, which takes the 33 ms of §3.2
  off the start of the sound.

## 10. Unverified

- Everything on an iOS device: hardware decode, the number of decoders a device grants at
  once, transport-security exceptions for addresses, the Local Network prompt, and playback.
- A SwiftUI interface, one target for iOS and macOS, and display on either platform (§4). §3.5
  does not record that its apps were SwiftUI or that one target served both.
- A picture drawn through `AVSampleBufferVideoRenderer` and its receiver, and the receiver's
  report of a lost decoder in its `enqueue` results (§1); every decode above ended at a pixel
  buffer. The library of §3.5 was built for iOS 17 and macOS 14, not for the deployment targets
  of §4.
- A view that decodes and hides the frames a session opens with, and shows the picture from the
  first frame that arrives no faster than it was recorded (§4).
- Recovery from a lost decoder by a flush of the receiver and a fresh session (§4).
- A grid of sub streams with one decoder per visible tile, beyond the three streams that decoded
  at once on a Mac (§3.1, §4).
- Whether a `CMReadySampleBuffer<CMSampleBuffer.DynamicContent>` made over a sample's range of a
  message the app holds, by any route of §1, keeps those bytes where they lie or copies them.
  `LiveDecoder` places the sample without copying the message
  (`crates/iris-client/src/live/decoder.rs`). What the platform's buffers do with the range was not
  checked.
- That `&[u8]` crosses from Swift into Rust without a copy, beyond the dated observation of §3.5
  (§4).
- Sound against picture, live and recorded, as heard.
- The receivers of both renderers on one synchronizer (`sampleBufferReceiver(adding:)`), and the
  route the synchronizer's header names in their place, a renderer's `sampleBufferReceiver()`
  and the synchronizer's `addReceiver(_:)`, which neither SDK's Swift interface declares (§1).
- Whether a view that shows frames on arrival moves evenly enough.
- AVPlayer's position against the sum of `#IRIS-FRAGMENT` ticks.
- The camera's multivariant playlist with its media fetched from the daemon directly.
- One recorded window at a time for each client, against the daemon's bound on streaming media
  answers shared by every client (§3.2, §4).
- The one live session that ran at half rate (§3.1).
- Whether Photos accepts an export.
- `mediastreamvalidator` on any playlist; the tool was not on the test machine.
- `xcodebuild test` on a simulator, which did not start on the Xcode used.
- The XCFramework built by `cargo rustc --crate-type staticlib` for five targets and wrapped in a
  Swift package, beyond the dated build of §3.5, whose script was not kept (§4).
- Signing a build for a device, with the signing identifiers in a file git ignores (§4). §3.5
  signed nothing for a device.
- The generated Swift as a separate package target, and Swift tests that are not hosted in the
  app, beyond the dated observations of §3.5 (§4).
- Where the Swift tree sits: outside `crates/`, with its build steps as `iris-tools` subcommands
  (§4). The tree holds no Swift file.
- The transport policy that a Swift transport must apply, in each of the five parts that §5 lists.
- A background `URLSession` that finishes a download after the app is suspended (§4).
- The statement of the app that the bearer and the video travel unencrypted over plain HTTP, and
  the verification of an `https` origin by the system's trust (§4).
- A reverse proxy under a private certificate authority, which would need the resource loader to
  carry the media (§4).
- The declarations of the app for transport security and Local Network privacy, in an app on
  macOS as on iOS (§3.4, §4).
- How the app tells a Local Network denial, a block by transport security, and a daemon that does
  not answer apart (§3.4, §6).
- Keeping the bearer in the Keychain (§4).
- An `hev1` stream whose `hvcC` holds no parameter set and whose samples carry the sets.
  `DecoderConfig::parse` refuses such an init segment (§11). The daemon writes `hvc1` alone, and no
  stream of that kind was read.
- `DecoderConfig::parse` on the init segments of the test daemon, beyond the dated run of
  2026-11-15, whose command was not kept (§11).
- The rewrite and the multivariant playlist of `iris_client::vod` in AVPlayer, and the bodies of
  `iris_client::playback` through a resource loader (§4).
- `iris_client::api` and `iris-transport` against a daemon, in any record of this document (§11).
- That tunnels and overlay networks number their peers from the ranges of RFC 1918 and RFC 6598
  (§4). For `100.64/10` that is the reason §4 gives for admitting the range to plain HTTP.
- `iris-client` under UniFFI and in a Swift app (§4). §3.5 put `iris-core` and `iris-mp4` there.
- How the code that UniFFI generates meets `STANDARDS.md` §6, and so how `iris-ffi` is built (§4).
- The clause text of ISO/IEC 14496-1, 14496-3, 14496-12, 14496-14, and 14496-15, none of which was
  read (`docs/standards/README.md`).
- Against that text, how the fragment reader treats the flags of the boxes it reads, and the rule
  that `hvc1` carries every parameter set in `hvcC` (§1).
- Against that text, the descriptors that the `esds` reader walks and the rates it gives the
  frequency indices (§1).
- That the reference software of MPEG publishes the `hvcC` layout (§1). No source is recorded for
  it.
- That a commercial recorder's phone client limits itself to six feeds (§2).
- That a recorder that composites a grid on the server re-encodes (§2).
- That Element X ships Rust through UniFFI as an XCFramework inside a Swift package (§2).

## 11. What is built

The Rust parts of §5 are built in `iris-mp4`, `iris-client`, and `iris-transport`. The Linux viewer
uses all of them but the decoder configuration and the live decoder. The `README.md` of each crate
states what the crate provides, its invariants, and how its tests run. The rustdoc of each item
states its contract. This section records what that code was checked against beyond fixtures
written with it: a daemon, a file that another program wrote, a tool, a clause, another crate's
document, or a platform. A record gives its date, and its command where one was kept.

- **Decoder configuration** (§4, §5), in `iris_mp4::decoder_config`
  (`crates/iris-mp4/src/decoder_config.rs`). No clause of ISO/IEC 14496 was read for its readers
  (§1). The fuzz target `decoder_config` runs them on arbitrary bytes. They were checked against a
  daemon and against ffmpeg:
  - On 2026-11-15 `DecoderConfig::parse` read the init segments of the test daemon. The main and sub
    streams gave HEVC at the sizes the daemon's API reports for them, 3840×2160 and 1920×1080, each
    with a VPS, an SPS, and a PPS. The audio stream gave AAC-LC at 64,000 Hz in one channel, the
    configuration that §3.1 decoded. The tree records no command for this run and has no program
    that repeats it. The result is a dated observation, **unverified** beyond it. The daemon writes
    `hvc1` alone (`third_party/retina/src/codec/mod.rs`).
  - On 2026-11-17 `DecoderConfig::parse` read an `hev1` init segment that ffmpeg 9.0.2 wrote with
    libx265 4.3+1-e9b8812 from one synthetic 64×64 frame. Its `hvcC` holds the three sets in arrays
    whose `array_completeness` bit is clear. The reader gave HEVC at 64×64, with the VPS, SPS, and
    PPS of 24, 41, and 7 bytes that ffmpeg's `hevc_mp4toannexb` put before the frame. The test
    `crates/iris-mp4/tests/ffmpeg_hev1.rs` has the first 820 bytes of the file. These commands wrote
    the file and its Annex B stream:

    ```text
    ffmpeg -f lavfi -i testsrc=size=64x64:rate=1 -frames:v 1 -pix_fmt yuv420p \
        -c:v libx265 -x265-params log-level=error:info=0 -tag:v hev1 \
        -movflags +frag_keyframe+empty_moov -fflags +bitexact \
        -flags:v +bitexact -map_metadata -1 -f mp4 hev1.mp4
    ffmpeg -i hev1.mp4 -c:v copy -bsf:v hevc_mp4toannexb -f hevc annexb.hevc
    ```
  - An `hev1` entry whose `hvcC` holds no parameter set is refused with
    `HvccError::MissingParameterSet`, because the reader takes the sets from the record alone. No
    stream of that kind was read (§10).
  - On 2026-11-18 `DecoderConfig::parse` read an `mp4a` init segment that ffmpeg 9.0.2 wrote with
    its native AAC encoder from 0.1 s of a synthetic 440 Hz tone at 48 kHz in two channels. Its
    `esds` gives the length of every descriptor in four bytes, where the `esds` of the daemon gives
    each in one. The reader gave the AudioSpecificConfig `11 90 56 e5 00`, 48,000 Hz, and two
    channels. ffprobe reports the same for the stream, as its extradata, `sample_rate`, and
    `channels`. The test `crates/iris-mp4/tests/ffmpeg_mp4a.rs` has the first 696 bytes of the file.
    These commands wrote the file and gave ffprobe's report:

    ```text
    ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.1 -ac 2 \
        -c:a aac -b:a 128k -movflags +frag_keyframe+empty_moov \
        -fflags +bitexact -flags:a +bitexact -map_metadata -1 -f mp4 mp4a.mp4
    ffprobe -v error -show_streams -show_data mp4a.mp4
    ```

  - Mutation testing. On 2026-11-22, on Linux, `just mutants` ran over `decoder_config.rs` and
    `boxes.rs` under `crates/iris-mp4/src/`. It tested 164 mutants: 155 caught, 6 unviable, 3
    missed. No input tells a missed mutant from the code. Two put `^` for `|` between operands
    that share no bit, in `descriptor` and in `sampling_rate`. The third builds a `BoxWriter`
    without its initial capacity.
  - On 2026-11-16 ffmpeg 9.0.2 decoded the video and the audio of the test daemon by the parameter
    sets and the AudioSpecificConfig that `DecoderConfig::parse` gave (§3.6).
- **The live decoder** (§4, §5), `iris_client::live::LiveDecoder`
  (`crates/iris-client/src/live/decoder.rs`). On 2026-11-16 it read every live feed of the test
  daemon for 12 s. It refused nothing, and ffmpeg decoded every sample it placed (§3.6). Its unit
  tests run over messages built with the encoders of `iris-mp4`, not over a daemon's bytes. The fuzz
  target `live_sample` checks that each answer is the one a model of the decoder's rules expects,
  and that each sample lies inside its message. On 2026-11-22, on Linux, `just mutants` tested 23
  mutants of the file: 21 caught, 2 unviable, none missed.
- **The playlist and fragment rules** (§4, §5), in `iris_client::vod`
  (`crates/iris-client/src/vod.rs`) and `iris_client::live::media_time`. The Linux viewer uses both.
  The fuzz targets `vod_playlist` and `live_fragment` run them on arbitrary bytes. No record in this
  document runs the rewrite or the multivariant playlist of `iris_client::vod` through AVPlayer, so
  both are **unverified** there (§10). On 2026-11-22, on Linux, `just mutants` tested 35 mutants
  of `crates/iris-client/src/vod.rs`: 33 caught, 2 unviable, none missed.
- **The API origin** (§4, §5), in `iris_client::origin` (`crates/iris-client/src/origin.rs`). The
  tests of `PlainHttp::PrivateAddresses` take the first and the last address of each range from RFC
  1918 §3 and RFC 6598 §7 (§1), with the address on either side of each range.
- **The requests and answers of the API** (§4, §5), in `iris_client::api`
  (`crates/iris-client/src/api.rs` and the files under `crates/iris-client/src/api/`). Its tests
  check each request builder against the route table of `crates/iris-api/README.md`. The fuzz target
  `client_answers` runs `HttpRequest::body_to_read` and every answer check over any status, content
  type, and body. No record in this document runs the builders and the checks against a daemon:
  **unverified** there.
- **The Linux viewer's transport** (§4, §5), the crate `iris-transport` (`crates/iris-transport`).
  Its tests run against fixture servers and loopback WebSockets in the same crate
  (`crates/iris-transport/src/fixture.rs`). The transport against a daemon is **unverified** in this
  document. Three checks reach outside the crate:
  - The platform's certificate verifier. The test
    `a_wss_session_refuses_a_server_the_platform_does_not_trust` connects to a `wss` server with a
    self-signed certificate. On Linux the verifier refuses it with `UnknownIssuer`, which the test
    asserts on Linux alone. macOS refuses it with another reason. On 2026-11-21
    `cargo test --locked -p iris-transport` passed on a Mac: 176 unit tests and 7 doctests. On the
    same date `just check` passed on a Linux host, where the crate ran 178 unit tests.
  - The trace that tungstenite writes. A feed's bearer travels in its WebSocket handshake.
    tungstenite writes the whole request at trace level, `Authorization` included, under the target
    `tungstenite::handshake::client`. `iris_transport::is_sensitive_log_target`
    (`crates/iris-transport/src/feed.rs`) names that target, and the Linux viewer's output layer
    drops it. The tests `tungstenite_traces_the_bearer_under_the_sensitive_target_alone`,
    `a_layer_filtered_by_the_guard_holds_no_bearer`, and
    `a_layer_filtered_by_the_guard_keeps_the_handshakes_other_lines` run a handshake with a sentinel
    bearer against a loopback listener that answers 400. A subscriber records every target at trace
    level. The one line with the bearer is under that target, so a tungstenite that wrote the
    request elsewhere fails the first test.
  - Mutation testing. On 2026-11-22, on Linux, `just mutants` ran over the three source files with
    the client, the export, and the feed loop: `client.rs`, `export.rs`, and `feed.rs` under
    `crates/iris-transport/src/`. It tested 109 mutants: 77 caught, 32 unviable, none missed.
- **The timeline model** (§4, §5, §6), in `iris_client::timeline_model`
  (`crates/iris-client/src/timeline_model.rs`). The Linux viewer draws its timeline and seeks
  through the model. That the position of AVPlayer equals the sum of the fragments' ticks is
  **unverified** (§10).
- **The playback selection** (§4, §5, §6), in `iris_client::playback`
  (`crates/iris-client/src/playback.rs`). The Linux viewer serves each body of a `PlaybackSnapshot`
  at its name on a loopback endpoint, and GStreamer fetches it there. That a resource loader hands
  the same bodies to AVPlayer is **unverified** (§10). On 2026-11-22, on Linux, `just mutants`
  tested 40 mutants of the file: 37 caught, 3 unviable, none missed.
- **The sound placement rule** (§4, §5, §6), in `iris_client::sound_clock`
  (`crates/iris-client/src/sound_clock.rs`). `13-audio.md` §10.4 has the rule and its dated
  measurements in the Linux viewer. Sound against picture through Apple's renderers is
  **unverified** (§10).
- **The live protocol, the reconnect policy, and the byte budget** (§4, §5), in `iris_client::live`
  (`crates/iris-client/src/live.rs` and the files under `crates/iris-client/src/live/`).
  `LIVE_MESSAGE_BYTES` has the value of the daemon's `MAX_LIVE_MESSAGE_BYTES`
  (`crates/iris-api/src/live.rs`). `LiveDecoder` reads each message through `live_message`, so the
  run of §3.6 read every message of the daemon's feeds through the framing. The fuzz target
  `live_message` runs the framing on arbitrary bytes. No record in this document checks the
  reconnect policy or the byte budget against a daemon.
- **The dependencies of `iris-client`** (§4, §5), in `crates/iris-client/Cargo.toml`, which names no
  feature, no optional dependency, and no dev-dependency. The transport that needs networking and
  TLS is `iris-transport`. On 2026-11-16, and again on 2026-11-18, on a Mac,
  `cargo tree --locked -p iris-client -e normal` listed as the crate's direct dependencies `http`,
  `iris-core`, `iris-mp4`, `serde`, `serde_json`, `thiserror`, and `url`. With `-e normal,build,dev`
  the whole tree had no networking or TLS crate, no `-sys` crate, and no `cc`, so neither the crate
  nor its tests compile C. With `-e normal` it listed under `iris-mp4`, beyond `iris-core` and
  `thiserror`, the crates `bytes`, `crc-fast`, `digest`, `crypto-common`, `generic-array`,
  `typenum`, and `spin`, all Rust. On both dates
  `cargo check --locked --target aarch64-apple-ios -p iris-client` passed.
- **Mutation testing of `iris-client`** (§5). On 2026-11-22, on Linux, `just mutants` ran over
  sixteen source files of `crates/iris-client/src`: `vod.rs`, `timeline_model.rs`, `playback.rs`,
  `sound_clock.rs`, `origin.rs`, the six files of `api/`, and the five files of `live/`. It tested
  550 mutants: 492 caught, 58 unviable, none missed.
- **The tests of `iris-client` on three targets** (§5). On 2026-11-22
  `cargo test --locked -p iris-client` passed on a Mac, and `just check` passed on a Linux host: 455
  unit tests and 39 doctests of the crate on each. The `cross-test` recipe of the `justfile` runs
  the crate's tests as 64-bit ARM code, with the tests of the other crates without C dependencies.
  They run as `aarch64-unknown-linux-musl` code, on CI's 64-bit ARM runner, and elsewhere under
  user-mode emulation. On 2026-11-17
  `cargo tree --locked --target aarch64-unknown-linux-musl -e normal,dev,build -p iris-client`
  listed no `cc` crate and no `-sys` crate, so nothing in the crate's tests compiles C there. On the
  same date `just cross-test`, run on an x86-64 Linux host, passed 945 tests under
  `qemu-aarch64-static`. Of those, 282 were the crate's: 264 unit tests and 18 doctests. On
  2026-11-18 `just cross-test`, run on a Linux host, passed 1,097 tests. Of those, 420 were the
  crate's: 382 unit tests and 38 doctests. The log of that run does not record the host's
  architecture, so it does not say whether the tests ran as native code or under
  `qemu-aarch64-static`. The 1,097 is the sum of the `passed` counts on the
  `test result` lines of the run, with two lines left out. Two tests of `iris-store` each run their
  test binary again under `unshare`, and each of those second processes prints a line for its one
  test. The record of 2026-11-17 does not say how its 945 was counted. In both runs 3 tests of other
  crates were `#[ignore]`d.

## Sources

- Apple, "NSAllowsLocalNetworking":
  <https://developer.apple.com/documentation/bundleresources/information-property-list/nsapptransportsecurity/nsallowslocalnetworking>
- Apple, TN3179 "Understanding local network privacy":
  <https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy>
- Apple developer forums, thread 20421 (the unsupported header option) and thread 764636
  (low-latency playlists through a resource loader):
  <https://developer.apple.com/forums/thread/20421>,
  <https://developer.apple.com/forums/thread/764636>
- UniFFI user guide for version 0.32, Swift bindings and futures:
  <https://mozilla.github.io/uniffi-rs/0.32/>
- Mozilla application-services, Swift package distribution:
  <https://mozilla.github.io/application-services/book/design/swift-package-manager.html>
- Home Assistant iOS, `CameraHLSAssetLoader.swift`:
  <https://github.com/home-assistant/iOS/tree/main/Sources/App/Cameras/CameraPlayer>
- Moonfire NVR, API reference:
  <https://github.com/scottlamb/moonfire-nvr/blob/master/ref/api.md>
- go2rtc: <https://github.com/AlexxIT/go2rtc>
- Frigate, live view: <https://docs.frigate.video/configuration/live>
- AetherEngine, `SampleBufferRenderer.swift`: <https://github.com/superuser404notfound/AetherEngine>
- Tender for Frigate, `NativeLivePlayer.swift`: <https://github.com/sweebee/tender-for-frigate>
- IPCamKit, the CameraViewer example: <https://github.com/steelbrain/IPCamKit>
- MPEG file format conformance, box syntax:
  <https://github.com/MPEGGroup/FileFormatConformance>
- ITU-T H.265 (V11) (01/2026), High efficiency video coding, freely available from ITU:
  <https://www.itu.int/rec/T-REC-H.265-202601-I>
- ITU, "ITU Copyright Information", read on 2026-11-18:
  <https://www.itu.int/home/copyright/index.html>
- Apple, "Developer account overview", read on 2026-11-18:
  <https://developer.apple.com/help/account/basics/about-your-developer-account>
