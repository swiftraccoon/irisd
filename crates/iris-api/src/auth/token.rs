//! The pre-authentication parsers of the bearer middleware: the `token` query
//! parameter and the encoded capability. Pure functions over client input,
//! included by the `api_auth` fuzz target as well as the crate.

/// The version field every capability starts with.
pub(crate) const CAPABILITY_VERSION: &str = "v1";
/// Length of a capability's HMAC-SHA256 tag.
pub(crate) const CAPABILITY_TAG_BYTES: usize = 32;
/// Longest encoded capability accepted before decoding.
pub(crate) const MAX_ENCODED_CAPABILITY_BYTES: usize = 96;

/// Splits an encoded capability into its expiry and tag: `v1.<expires>.<hex>`
/// with exactly three fields, a decimal expiry, and a 32-byte tag; `None` for
/// anything else, including an empty or over-long text.
pub(crate) fn parse_capability(encoded: &str) -> Option<(u64, [u8; CAPABILITY_TAG_BYTES])> {
    if encoded.is_empty() || encoded.len() > MAX_ENCODED_CAPABILITY_BYTES {
        return None;
    }
    let mut fields = encoded.split('.');
    if fields.next()? != CAPABILITY_VERSION {
        return None;
    }
    let expires_at = fields.next()?.parse().ok()?;
    let tag = decode_hex(fields.next()?)?;
    if fields.next().is_some() || tag.len() != CAPABILITY_TAG_BYTES {
        return None;
    }
    let mut out = [0; CAPABILITY_TAG_BYTES];
    out.copy_from_slice(&tag);
    Some((expires_at, out))
}

/// Decodes lowercase or uppercase hexadecimal; `None` for an odd length or a
/// non-hexadecimal byte.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

/// Extracts `token=` from a raw query string, decoding percent escapes and
/// rejecting blank or excessively long credentials before allocation.
pub(crate) fn query_token(query: Option<&str>) -> Option<String> {
    let raw = query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))?;
    if raw.is_empty() || raw.len() > MAX_ENCODED_CAPABILITY_BYTES {
        return None;
    }
    let decoded = percent_decode(raw)?;
    (!decoded.trim().is_empty()).then_some(decoded)
}

/// Decodes `%XX` escapes and `+` as a space; `None` for a malformed escape or
/// a result that is not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                out.push(value);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}
