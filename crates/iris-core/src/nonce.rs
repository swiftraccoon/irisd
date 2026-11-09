//! Random tokens for protocol exchanges.

/// A fresh 64-bit random value as sixteen lowercase hexadecimal characters,
/// for a digest-authentication client nonce.
///
/// The operating system's random source backs it (`getrandom`), so a camera
/// that knows when a request left cannot narrow the value, which is what the
/// client nonce defends against (RFC 7616 §5.9 and §5.12).
///
/// ```
/// let nonce = iris_core::nonce::client_nonce();
/// assert_eq!(nonce.len(), 16);
/// assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
/// ```
///
/// # Panics
///
/// Panics when the operating system's random source is unavailable, which
/// `getrandom` documents as a system that cannot run the process safely.
#[must_use]
pub fn client_nonce() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    let mut text = String::with_capacity(16);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_hexadecimal_and_distinct() {
        let first = client_nonce();
        let second = client_nonce();
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
