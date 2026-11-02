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
});
