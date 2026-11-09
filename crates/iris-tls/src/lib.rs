//! The TLS trust every irisd client shares.
//!
//! The viewer and the tools reach a daemon behind a TLS reverse proxy over
//! HTTPS and `wss`. Both use one `rustls` client configuration from
//! [`client_config`]: the platform's certificate store through
//! `rustls-platform-verifier`, extended by the authorities in an optional PEM
//! file for a proxy with a private CA. One configuration means one trust
//! decision: a certificate the API reads accept, the live feeds accept too.
//!
//! # Example
//!
//! ```
//! let config = iris_tls::client_config(None)?;
//! assert_eq!(config.alpn_protocols, Vec::<Vec<u8>>::new(), "the caller picks the protocol");
//! # Ok::<(), iris_tls::TlsError>(())
//! ```

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use rustls_platform_verifier::Verifier;
use thiserror::Error;

/// Longest certificate authority file read; a bundle of a few hundred
/// certificates stays far below it.
pub const MAX_CA_FILE_BYTES: u64 = 1 << 20;

/// Why a client configuration could not be built.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TlsError {
    /// The certificate authority file could not be read.
    #[error("reading the certificate authority file {path}")]
    ReadCaFile {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The certificate authority file is longer than [`MAX_CA_FILE_BYTES`].
    #[error("the certificate authority file {path} exceeds {MAX_CA_FILE_BYTES} bytes")]
    CaFileTooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The certificate authority file holds no certificate.
    #[error("the certificate authority file {path} holds no PEM certificate")]
    CaFileEmpty {
        /// The file.
        path: PathBuf,
    },
    /// The certificate authority file holds a malformed PEM section.
    #[error("the certificate authority file {path} is not valid PEM")]
    CaFilePem {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    /// The certificate authority file holds a PEM certificate `rustls`
    /// cannot use.
    #[error("the certificate authority file {path} holds a certificate rustls cannot use")]
    CaFileCertificate {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rustls::Error,
    },
    /// `rustls` refused the configuration or the platform verifier, which on
    /// Linux includes a host without any CA certificate in its system store.
    #[error("building the TLS client configuration")]
    Rustls(#[from] rustls::Error),
}

/// Builds the client configuration: the platform's trust store, plus the
/// certificate authorities in `ca_file` (PEM, at most [`MAX_CA_FILE_BYTES`])
/// when given. The configuration carries no ALPN protocols; HTTP clients and
/// WebSocket connections add what they need.
///
/// On Linux the platform store is the distribution's CA bundle, read through
/// `rustls-native-certs`; there is no bundled fallback, so a host without a
/// bundle fails with [`TlsError::Rustls`] unless `ca_file` supplies the roots
/// it needs.
///
/// # Errors
///
/// [`TlsError::ReadCaFile`], [`TlsError::CaFileTooLarge`],
/// [`TlsError::CaFileEmpty`], [`TlsError::CaFilePem`], or
/// [`TlsError::CaFileCertificate`] for an unusable `ca_file`;
/// [`TlsError::Rustls`] when no root certificate loads or `rustls` refuses the
/// configuration.
pub fn client_config(ca_file: Option<&Path>) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = match ca_file {
        Some(path) => Verifier::new_with_extra_roots(read_ca_file(path)?, Arc::clone(&provider))
            .map_err(|source| TlsError::CaFileCertificate {
                path: path.to_path_buf(),
                source,
            })?,
        None => Verifier::new(Arc::clone(&provider))?,
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Reads at most [`MAX_CA_FILE_BYTES`] of PEM certificates from `path`.
fn read_ca_file(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let read_error = |source| TlsError::ReadCaFile {
        path: path.to_path_buf(),
        source,
    };
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(read_error)?
        .take(MAX_CA_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > MAX_CA_FILE_BYTES {
        return Err(TlsError::CaFileTooLarge {
            path: path.to_path_buf(),
        });
    }
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsError::CaFilePem {
            path: path.to_path_buf(),
            source,
        })?;
    if certificates.is_empty() {
        return Err(TlsError::CaFileEmpty {
            path: path.to_path_buf(),
        });
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::SocketAddr;

    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;

    use super::*;

    /// Bounds every network step of a test.
    const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

    /// A private CA and a server certificate it issued for 127.0.0.1.
    struct Authority {
        ca_pem: String,
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivatePkcs8KeyDer<'static>,
    }

    fn private_authority() -> Authority {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_cert.pem();
        let issuer = Issuer::new(ca_params, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let server_cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .signed_by(&server_key, &issuer)
            .unwrap();
        Authority {
            ca_pem,
            server_chain: vec![server_cert.der().clone(), ca_cert.der().clone()],
            server_key: PrivatePkcs8KeyDer::from(server_key.serialize_der()),
        }
    }

    /// A TLS server on loopback answering every HTTP/1.1 request with 204.
    async fn https_fixture(authority: &Authority) -> SocketAddr {
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                authority.server_chain.clone(),
                authority.server_key.clone_key().into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
                        if tls.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let _ = tls
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        address
    }

    fn ca_file(dir: &tempfile::TempDir, pem: &str) -> PathBuf {
        let path = dir.path().join("private-ca.pem");
        std::fs::write(&path, pem).unwrap();
        path
    }

    fn https_client(config: &Arc<ClientConfig>) -> reqwest::Client {
        reqwest::Client::builder()
            .tls_backend_preconfigured((**config).clone())
            .connect_timeout(TEST_DEADLINE)
            .timeout(TEST_DEADLINE)
            .build()
            .unwrap()
    }

    #[test]
    fn the_platform_store_alone_builds_a_configuration() {
        let config = client_config(None).unwrap();
        assert_eq!(config.alpn_protocols, Vec::<Vec<u8>>::new());
    }

    #[tokio::test]
    async fn a_private_authority_is_trusted_only_through_the_ca_file() {
        let authority = private_authority();
        let address = https_fixture(&authority).await;
        let url = format!("https://127.0.0.1:{}/api/health", address.port());

        let platform_only = https_client(&client_config(None).unwrap());
        let error = platform_only
            .get(&url)
            .send()
            .await
            .expect_err("a private authority is not in the platform store");
        assert!(error.is_connect(), "{error:#}");

        let dir = tempfile::tempdir().unwrap();
        let path = ca_file(&dir, &authority.ca_pem);
        let with_ca = https_client(&client_config(Some(&path)).unwrap());
        let response = with_ca
            .get(&url)
            .send()
            .await
            .expect("the CA file makes the proxy trusted");
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);

        let other = private_authority();
        let other_dir = tempfile::tempdir().unwrap();
        let other_path = ca_file(&other_dir, &other.ca_pem);
        let wrong_ca = https_client(&client_config(Some(&other_path)).unwrap());
        let error = wrong_ca
            .get(&url)
            .send()
            .await
            .expect_err("another private authority does not vouch for the server");
        assert!(error.is_connect(), "{error:#}");
    }

    #[test]
    fn ca_file_problems_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        assert!(matches!(
            client_config(Some(&missing)).unwrap_err(),
            TlsError::ReadCaFile { path, .. } if path == missing
        ));

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(
            client_config(Some(&empty)).unwrap_err(),
            TlsError::CaFileEmpty { path } if path == empty
        ));

        let text = dir.path().join("text.pem");
        std::fs::write(&text, "not a certificate\n").unwrap();
        assert!(matches!(
            client_config(Some(&text)).unwrap_err(),
            TlsError::CaFileEmpty { .. }
        ));

        let truncated = dir.path().join("truncated.pem");
        std::fs::write(&truncated, "-----BEGIN CERTIFICATE-----\nAAAA\n").unwrap();
        assert!(matches!(
            client_config(Some(&truncated)).unwrap_err(),
            TlsError::CaFilePem { .. }
        ));

        let garbage = dir.path().join("garbage.pem");
        std::fs::write(
            &garbage,
            "-----BEGIN CERTIFICATE-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(matches!(
            client_config(Some(&garbage)).unwrap_err(),
            TlsError::CaFileCertificate { path, .. } if path == garbage
        ));

        let oversized = dir.path().join("oversized.pem");
        let mut file = File::create(&oversized).unwrap();
        file.write_all(&vec![b'-'; (MAX_CA_FILE_BYTES + 1) as usize])
            .unwrap();
        drop(file);
        assert!(matches!(
            client_config(Some(&oversized)).unwrap_err(),
            TlsError::CaFileTooLarge { path } if path == oversized
        ));
    }
}
