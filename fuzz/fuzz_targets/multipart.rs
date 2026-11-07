//! Fuzzes the multipart event-feed reader: any boundary and any chunking must never panic.
#![no_main]

use iris_events::multipart::{MultipartReader, boundary_from_content_type};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&len, rest)) = data.split_first() else {
        return;
    };
    let boundary_len = usize::from(len % 41).min(rest.len());
    let boundary = String::from_utf8_lossy(&rest[..boundary_len]).into_owned();
    let body = &rest[boundary_len..];
    let _ = boundary_from_content_type(&boundary);
    let mut reader = MultipartReader::new(&boundary);
    // Feed the body in chunks whose sizes come from the data itself.
    let mut pos = 0;
    while pos < body.len() {
        let chunk = usize::from(body[pos] % 17) + 1;
        let end = (pos + chunk).min(body.len());
        if reader.push(&body[pos..end]).is_err() {
            break;
        }
        pos = end;
    }

    // Semantic oracle: a bounded delimiter-free body must parse identically
    // in one push and under adversarial fragmentation.
    let payload: Vec<u8> = body
        .iter()
        .take(4096)
        .map(|byte| b'a' + byte % 26)
        .collect();
    let mut feed = format!(
        "--fuzz-boundary\r\nContent-Length:{}\r\n\r\n",
        payload.len()
    )
    .into_bytes();
    feed.extend_from_slice(&payload);
    feed.extend_from_slice(b"--fuzz-boundary\r\n");

    let mut whole = MultipartReader::new("fuzz-boundary");
    let expected = whole.push(&feed).expect("bounded synthetic feed");
    let mut fragmented = MultipartReader::new("fuzz-boundary");
    let mut actual = Vec::new();
    for chunk in feed.chunks(usize::from(len % 31) + 1) {
        actual.extend(fragmented.push(chunk).expect("bounded synthetic feed"));
    }
    assert_eq!(actual, expected);
    assert!(
        fragmented.examined_for_fuzzing() <= feed.len().saturating_mul(4),
        "fragmented scan work exceeded a linear bound"
    );
});
