//! Fuzzes retina's RTSP message parser, which reads every camera reply and
//! interleaved-data header before irisd sees them.
//!
//! One byte stream is parsed twice: from one contiguous buffer, and in
//! fuzz-chosen chunks through the two-slice ring-buffer views the RTSP client
//! reads from. Both parses must yield the same messages and end the same way,
//! except that the chunked parse may stop earlier at the size limit, and no
//! request or response may exceed that limit. Each input runs under the
//! production limit and under a fuzz-chosen one.
#![no_main]

use libfuzzer_sys::fuzz_target;
use retina::inputs::{Input, Split};
use retina::rtsp::msg::Message;
use retina::rtsp::parse::{FeedError, Parser};

/// The head-plus-body limit retina sets on production RTSP connections. It
/// mirrors `retina::rtsp::parse::MAX_CONTROL_MESSAGE_BYTES`, which is private
/// to the crate.
const PRODUCTION_LIMIT: usize = 1024 * 1024;

/// The innermost error context of a size-limit rejection.
const TOO_LARGE: &str = "message-too-large";

/// How a parse of the stream ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// Every byte belonged to a complete message or to keep-alive line breaks.
    Clean,
    /// The stream stopped inside a message.
    Truncated,
    /// The parser rejected the stream; `too_large` marks a size-limit rejection.
    Invalid { too_large: bool },
}

/// Complete messages with their bodies, in stream order, and how the stream ended.
type Parsed = (Vec<(Message, Vec<u8>)>, End);

fuzz_target!(|data: &[u8]| {
    let Some((&[limit_low, limit_high, control_len], rest)) = data.split_first_chunk::<3>() else {
        return;
    };
    let (control, stream) = rest.split_at(usize::from(control_len % 16).min(rest.len()));
    let fuzzed_limit = usize::from(u16::from_le_bytes([limit_low, limit_high]));
    for limit in [PRODUCTION_LIMIT, fuzzed_limit] {
        let contiguous = parse_contiguous(stream, limit);
        let chunked = parse_in_chunks(stream, limit, control);
        check_agreement(&contiguous, &chunked, limit <= stream.len(), limit);
    }
});

/// Checks that chunked parsing agrees with contiguous parsing.
///
/// The one permitted difference is a size-limit rejection that comes earlier in
/// the chunked parse. The parser checks the limit against the bytes it can see,
/// so keep-alive line breaks (or an empty view, under a zero limit) that reach the
/// limit fail before the interleaved-data frame that follows them arrives, and the
/// limit does not cover such a frame. That needs a stream at least as long as the
/// limit (`limit_reachable`).
fn check_agreement(contiguous: &Parsed, chunked: &Parsed, limit_reachable: bool, limit: usize) {
    let (contiguous_messages, contiguous_end) = contiguous;
    let (chunked_messages, chunked_end) = chunked;
    if limit_reachable && *chunked_end == (End::Invalid { too_large: true }) {
        assert!(
            contiguous_messages.starts_with(chunked_messages),
            "chunked parsing hit the {limit}-byte limit after messages that contiguous parsing \
             did not yield: chunked {chunked:?}, contiguous {contiguous:?}"
        );
        return;
    }
    let both_invalid = matches!(
        (contiguous_end, chunked_end),
        (End::Invalid { .. }, End::Invalid { .. })
    );
    assert!(
        chunked_messages == contiguous_messages && (both_invalid || chunked_end == contiguous_end),
        "chunked parsing diverged from contiguous parsing under a {limit}-byte limit: \
         chunked {chunked:?}, contiguous {contiguous:?}"
    );
}

/// Parses `stream` from one contiguous buffer.
fn parse_contiguous(stream: &[u8], limit: usize) -> Parsed {
    let mut parser = Parser::builder().max_message_size(limit).build();
    let mut input = stream;
    let mut messages = Vec::new();
    loop {
        let before = input.len();
        match parser.feed(&mut input) {
            Ok(Some((message, body))) => {
                check_message(&message, body.len(), before - input.len(), limit);
                messages.push((message, body.to_vec()));
            }
            Ok(None) => return (messages, End::Clean),
            Err(FeedError::Incomplete(_)) => return (messages, End::Truncated),
            Err(FeedError::Invalid(invalid)) => {
                let too_large = invalid.context.first() == Some(&TOO_LARGE);
                return (messages, End::Invalid { too_large });
            }
        }
    }
}

/// Parses `stream` the way the RTSP client reads its socket: bytes arrive in
/// chunks, and every attempt sees the unconsumed bytes as a ring buffer's two
/// halves.
fn parse_in_chunks(stream: &[u8], limit: usize, control: &[u8]) -> Parsed {
    let mut parser = Parser::builder().max_message_size(limit).build();
    let mut messages = Vec::new();
    // Offsets into `stream`: where the message in progress starts, how far the
    // client has consumed, and how many bytes have arrived.
    let mut message_start = 0;
    let mut consumed = 0;
    let mut received = 0;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let unconsumed = &stream[consumed..received];
        let split = usize::from(control_byte(control, attempt)) % (unconsumed.len() + 1);
        let (first, second) = unconsumed.split_at(split);
        let mut input = Split::new(first, second);
        let result = parser.feed(&mut input);
        let taken = unconsumed.len() - input.len();
        let truncated = match result {
            Ok(Some((message, body))) => {
                consumed += taken;
                check_message(&message, body.len(), consumed - message_start, limit);
                messages.push((message, body.to_cow().into_owned()));
                message_start = consumed;
                continue;
            }
            Err(FeedError::Invalid(invalid)) => {
                let too_large = invalid.context.first() == Some(&TOO_LARGE);
                return (messages, End::Invalid { too_large });
            }
            // Like the client, keep the line breaks the parser skipped while
            // idle, and drop the lines it consumed from an incomplete message.
            Ok(None) => false,
            Err(FeedError::Incomplete(_)) => {
                consumed += taken;
                true
            }
        };
        if received == stream.len() {
            let end = if truncated {
                End::Truncated
            } else {
                End::Clean
            };
            return (messages, end);
        }
        let chunk = usize::from(control_byte(control, attempt + 1) % 64) + 1;
        received = (received + chunk).min(stream.len());
    }
}

/// Checks what the parser guarantees for every complete message.
fn check_message(message: &Message, body_len: usize, message_len: usize, limit: usize) {
    assert!(message_len > 0, "a complete message consumed no input");
    match message {
        Message::Data(data) => assert_eq!(body_len, usize::from(data.body_len)),
        Message::Request(_) | Message::Response(_) => assert!(
            message_len <= limit,
            "a {message_len}-byte message passed the {limit}-byte limit"
        ),
    }
}

/// The control byte for `attempt`, cycling through `control`.
fn control_byte(control: &[u8], attempt: usize) -> u8 {
    if control.is_empty() {
        1
    } else {
        control[attempt % control.len()]
    }
}
