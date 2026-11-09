# iris-tls

The TLS client trust irisd's clients share. The viewer and the tools reach a daemon behind a
TLS reverse proxy over HTTPS and `wss`; both use one `rustls` client configuration from this
crate, so API reads and live feeds make the same trust decision.

## What it provides

- `client_config(ca_file)` — one `Arc<rustls::ClientConfig>` built on the platform's
  certificate store (`rustls-platform-verifier`), extended by every certificate in an optional
  PEM file for a proxy with a private certificate authority. The configuration carries no ALPN
  protocols; the caller adds what its protocol needs.
- `TlsError` — why a configuration could not be built: the CA file could not be read, is
  longer than `MAX_CA_FILE_BYTES` (1 MiB), holds no certificate, is not PEM, or holds a
  certificate `rustls` cannot use; or `rustls` refused the configuration, which on a Linux host
  without a CA bundle is how an empty platform store is reported.
- `ClientConfig` — the `rustls` type, re-exported so callers name it without depending on
  `rustls` themselves.

## How it is used

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tls = iris_tls::client_config(None)?;
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured((*tls).clone())
        .build()?;
    // For a WebSocket: tokio_tungstenite::Connector::Rustls(Arc::clone(&tls)).
    Ok(())
}
```

`iris-gui --ca-file PEM` and `iris-tools soak-sample --ca-file PEM` pass the file through.

## Invariants and limits

- The platform store is always consulted; a CA file adds authorities, never replaces them. On
  Linux the store is the distribution's CA bundle and there is no bundled fallback.
- A CA file is read through a 1 MiB bound and must hold at least one PEM certificate.
- Certificate verification is never disabled. A proxy's certificate must chain to the
  platform store or to the CA file for the hostname or address the client dials.

## Tests and measurements

The crate's tests build a private authority with `rcgen`, serve HTTPS on loopback through
`tokio-rustls`, and check that `reqwest` with the configuration connects only when the CA file
names that authority, and that a missing, empty, non-PEM, unusable-certificate, or oversized
file is reported. Building the platform-only configuration needs the host's CA bundle.
`crates/iris-gui/src/live.rs` runs the same check for `wss`.

## Further reading

- `docs/research/06-clients.md` §3 (remote viewing through a TLS proxy)
- `docs/research/07-architecture.md` §5 (remote access through a TLS reverse proxy) and §6
  (the security model)
