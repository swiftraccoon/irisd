# AGENTS.md — iris-tls

Scope: this crate only. Read the root `AGENTS.md` first. This crate exists so that every
client of the daemon makes one trust decision; keep it small and free of protocol code.

## Invariants

- `client_config` always includes the platform's certificate store; `--ca-file` adds
  authorities. Never add an option that disables verification.
- The CA file read is bounded (`MAX_CA_FILE_BYTES`) and the file must hold a certificate;
  report problems with the path through `TlsError`, a certificate `rustls` cannot use
  included (`TlsError::CaFileCertificate`).
- The crate re-exports `rustls::ClientConfig` so callers do not depend on `rustls`; callers
  hand the configuration to `reqwest` (`tls_backend_preconfigured`) and to
  `tokio-tungstenite` (`Connector::Rustls`). A client of the daemon uses this crate rather
  than a trust configuration of its own.

## Where things are

- `src/lib.rs` — `client_config`, `TlsError`, the bounded PEM read, and the tests (an `rcgen`
  authority served through `tokio-rustls`, fetched with `reqwest`).

## How to verify a change

```sh
cargo fmt -p iris-tls
cargo clippy -p iris-tls --all-targets -- -D warnings
cargo test -p iris-tls
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-tls --no-deps
```

The doctest and `the_platform_store_alone_builds_a_configuration` need the host's CA bundle.

## Pitfalls

- `rustls-platform-verifier` 0.7 on Linux loads the system store through
  `rustls-native-certs` and has no bundled fallback: with an empty store `client_config(None)`
  fails with `TlsError::Rustls`, and a `--ca-file` configuration trusts only the file's
  authorities. Hosts need the distribution's CA bundle.
- `reqwest::ClientBuilder::tls_backend_preconfigured` takes a `rustls::ClientConfig` by value
  of the same `rustls` version; clone the shared configuration out of its `Arc`.

## Related documents

- `docs/research/06-clients.md` §3, `07-architecture.md` §5 and §6.
- Dependents: `iris-gui`, `iris-tools`.
