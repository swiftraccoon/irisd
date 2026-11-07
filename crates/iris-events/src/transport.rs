//! SOAP HTTP transport with the camera's TLS verification policy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use diqwest::{DigestAuthSession, WithDigestAuth as _};
use futures_util::StreamExt as _;
use iris_core::id::CameraId;
use oxvif::transport::{Transport, TransportError};
use tracing::debug;
use url::Url;

use crate::endpoint::CameraEndpoint;

/// Maximum accepted SOAP response body on every HTTP status.
pub(crate) const MAX_SOAP_RESPONSE_LEN: usize = 1024 * 1024;

/// Keeps the upstream ONVIF request bound and digest authentication while
/// applying the same certificate policy as the CGI feed.
pub(crate) struct SoapTransport {
    client: reqwest::Client,
    digest: DigestAuthSession,
    origin: Url,
    camera: CameraId,
    rebase_logged: AtomicBool,
}

impl SoapTransport {
    pub(crate) fn new(endpoint: &CameraEndpoint) -> Result<Self, reqwest::Error> {
        let origin = Url::parse(&endpoint.base_url())
            .expect("CameraEndpoint::base_url always produces a valid HTTP(S) URL");
        Ok(Self {
            client: reqwest::Client::builder()
                .danger_accept_invalid_certs(!endpoint.tls_verify)
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            digest: DigestAuthSession::new(&endpoint.username, &endpoint.password),
            origin,
            camera: endpoint.camera.clone(),
            rebase_logged: AtomicBool::new(false),
        })
    }

    /// Rebases a camera-advertised service URL onto the configured origin
    /// before WS-Security credentials are constructed for it.
    ///
    /// Only `http` and `https` URLs without userinfo or a fragment are
    /// accepted. Their path and query are kept; the scheme, host, and port are
    /// always the configured ones. Cameras behind NAT or a port forward, or
    /// configured by host name, advertise addresses the recorder cannot or
    /// must not reach, and credentials only ever go to the operator's
    /// authority; an HTTPS configuration is never downgraded. A rebase is
    /// logged once per transport, at debug level, with the camera only.
    pub(crate) fn rebase_url(&self, candidate: &str) -> Result<Url, TransportError> {
        let parsed = Url::parse(candidate).map_err(|_| endpoint_policy_error())?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(endpoint_policy_error());
        }
        let mut rebased = self.origin.clone();
        rebased.set_path(parsed.path());
        rebased.set_query(parsed.query());
        let advertised_origin = (
            parsed.scheme(),
            parsed.host_str(),
            parsed.port_or_known_default(),
        );
        let configured_origin = (
            rebased.scheme(),
            rebased.host_str(),
            rebased.port_or_known_default(),
        );
        if advertised_origin != configured_origin
            && !self.rebase_logged.swap(true, Ordering::Relaxed)
        {
            debug!(
                camera = %self.camera,
                "camera advertises another service origin; using the configured one"
            );
        }
        Ok(rebased)
    }
}

#[async_trait]
impl Transport for SoapTransport {
    async fn soap_post(
        &self,
        url: &str,
        action: &str,
        body: String,
    ) -> Result<String, TransportError> {
        let url = self.rebase_url(url)?;
        let response = self
            .client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
            )
            .header(
                reqwest::header::USER_AGENT,
                concat!("irisd/", env!("CARGO_PKG_VERSION")),
            )
            .body(body)
            .send_digest_auth(&self.digest)
            .await
            .map_err(|error| match error {
                diqwest::error::Error::Reqwest(error) => TransportError::Http(error),
                _ => TransportError::HttpStatus {
                    status: 401,
                    body: String::new(),
                },
            })?;
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SOAP_RESPONSE_LEN as u64)
        {
            return Err(response_too_large_error());
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if chunk.len() > MAX_SOAP_RESPONSE_LEN.saturating_sub(bytes.len()) {
                return Err(response_too_large_error());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes).map_err(|_| TransportError::InvalidResponseEncoding)?;
        // SOAP faults use 400 or 500 and belong to the SOAP parser.
        match status {
            200 | 400 | 500 => Ok(body),
            _ => Err(TransportError::HttpStatus {
                status,
                body: String::new(),
            }),
        }
    }
}

fn endpoint_policy_error() -> TransportError {
    TransportError::EndpointPolicy
}

fn response_too_large_error() -> TransportError {
    TransportError::ResponseTooLarge {
        limit: MAX_SOAP_RESPONSE_LEN,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;

    fn endpoint_for(address: std::net::SocketAddr) -> CameraEndpoint {
        CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: address.ip().to_string(),
            http_port: address.port(),
            https: false,
            tls_verify: true,
            username: "test".to_owned(),
            password: String::new(),
        }
    }

    #[tokio::test]
    async fn soap_transport_keeps_digest_authentication_and_soap_faults() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8_192];
                let length = socket.read(&mut request).await.unwrap();
                let request = std::str::from_utf8(&request[..length])
                    .unwrap()
                    .to_ascii_lowercase();
                assert!(request.contains("action=\"urn:pull\""));
                if attempt == 0 {
                    socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"test\", nonce=\"test-nonce\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    assert!(request.contains("authorization: digest "));
                    socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfault").await.unwrap();
                }
            }
        });
        let endpoint = endpoint_for(address);
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            SoapTransport::new(&endpoint).unwrap().soap_post(
                &endpoint.base_url(),
                "urn:pull",
                "<request/>".to_owned(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(body, "fault");
        server.await.unwrap();
    }

    #[test]
    fn advertised_urls_are_rebased_onto_the_full_configured_origin() {
        let endpoint = endpoint_for("127.0.0.1:8080".parse().unwrap());
        let transport = SoapTransport::new(&endpoint).unwrap();
        for candidate in [
            "http://127.0.0.1:8080/onvif/events?x=1",
            "http://127.0.0.2:8080/onvif/events?x=1",
            "http://127.0.0.1:8081/onvif/events?x=1",
            "https://127.0.0.1:8080/onvif/events?x=1",
            "http://camera.example/onvif/events?x=1",
            "http://[2001:db8::1]:80/onvif/events?x=1",
        ] {
            assert_eq!(
                transport.rebase_url(candidate).unwrap().as_str(),
                "http://127.0.0.1:8080/onvif/events?x=1",
                "{candidate}"
            );
        }
        for candidate in [
            "http://user@127.0.0.1:8080/onvif/events",
            "http://user:secret@192.0.2.10/onvif/events",
            "http://127.0.0.1:8080/onvif/events#fragment",
            "http://192.0.2.10/onvif/events#",
            "file:///tmp/events",
            "ftp://127.0.0.1:8080/onvif/events",
            "not a url",
        ] {
            assert!(
                transport.rebase_url(candidate).is_err(),
                "accepted {candidate}"
            );
        }

        let mut https_endpoint = endpoint;
        https_endpoint.https = true;
        let https_transport = SoapTransport::new(&https_endpoint).unwrap();
        assert_eq!(
            https_transport
                .rebase_url("http://127.0.0.1:8080/onvif/events")
                .unwrap()
                .as_str(),
            "https://127.0.0.1:8080/onvif/events",
            "an authenticated HTTPS endpoint is never downgraded"
        );
    }

    #[test]
    fn a_camera_configured_by_host_name_keeps_its_name_for_advertised_addresses() {
        let mut endpoint = endpoint_for("127.0.0.1:8080".parse().unwrap());
        endpoint.host = "localhost".to_owned();
        let transport = SoapTransport::new(&endpoint).unwrap();
        for candidate in [
            "http://127.0.0.1:8080/onvif/event_service",
            "http://localhost/onvif/event_service",
            "http://localhost:8080/onvif/event_service",
        ] {
            assert_eq!(
                transport.rebase_url(candidate).unwrap().as_str(),
                "http://localhost:8080/onvif/event_service",
                "{candidate}"
            );
        }
    }

    async fn serve_once(status: u16, headers: &str, body: Vec<u8>) -> CameraEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let headers = headers.to_owned();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8_192];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(
                    format!("HTTP/1.1 {status} Test\r\n{headers}Connection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
        });
        endpoint_for(address)
    }

    #[tokio::test]
    async fn response_limit_accepts_exact_and_rejects_declared_limit_plus_one() {
        let exact = vec![b'x'; MAX_SOAP_RESPONSE_LEN];
        let endpoint =
            serve_once(200, &format!("Content-Length: {}\r\n", exact.len()), exact).await;
        let body = SoapTransport::new(&endpoint)
            .unwrap()
            .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
            .await
            .unwrap();
        assert_eq!(body.len(), MAX_SOAP_RESPONSE_LEN);

        for status in [200, 400, 500, 418] {
            let endpoint = serve_once(
                status,
                &format!("Content-Length: {}\r\n", MAX_SOAP_RESPONSE_LEN + 1),
                Vec::new(),
            )
            .await;
            let error = SoapTransport::new(&endpoint)
                .unwrap()
                .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                TransportError::ResponseTooLarge {
                    limit: MAX_SOAP_RESPONSE_LEN
                }
            ));
        }
    }

    #[tokio::test]
    async fn chunked_limit_is_exact_on_every_relevant_status() {
        fn chunked(body: &[u8]) -> Vec<u8> {
            let mut encoded = format!("{:x}\r\n", body.len()).into_bytes();
            encoded.extend_from_slice(body);
            encoded.extend_from_slice(b"\r\n0\r\n\r\n");
            encoded
        }

        let exact = vec![b'x'; MAX_SOAP_RESPONSE_LEN];
        let endpoint = serve_once(200, "Transfer-Encoding: chunked\r\n", chunked(&exact)).await;
        let body = SoapTransport::new(&endpoint)
            .unwrap()
            .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
            .await
            .unwrap();
        assert_eq!(body.len(), MAX_SOAP_RESPONSE_LEN);

        let oversized = vec![b'x'; MAX_SOAP_RESPONSE_LEN + 1];
        for status in [200, 400, 500, 418] {
            let endpoint = serve_once(
                status,
                "Transfer-Encoding: chunked\r\n",
                chunked(&oversized),
            )
            .await;
            let error = SoapTransport::new(&endpoint)
                .unwrap()
                .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                TransportError::ResponseTooLarge {
                    limit: MAX_SOAP_RESPONSE_LEN
                }
            ));
        }
    }

    #[tokio::test]
    async fn unexpected_status_body_and_invalid_encoding_stay_bounded() {
        let marker = "camera-controlled\nmultiline\rmarker";
        let endpoint = serve_once(
            418,
            &format!("Content-Length: {}\r\n", marker.len()),
            marker.as_bytes().to_vec(),
        )
        .await;
        let error = SoapTransport::new(&endpoint)
            .unwrap()
            .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("camera-controlled"));
        assert!(rendered.len() < 128);

        let invalid = vec![0xff; MAX_SOAP_RESPONSE_LEN];
        let endpoint = serve_once(
            200,
            &format!("Content-Length: {}\r\n", invalid.len()),
            invalid,
        )
        .await;
        let error = SoapTransport::new(&endpoint)
            .unwrap()
            .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
            .await
            .unwrap_err();
        assert!(matches!(error, TransportError::InvalidResponseEncoding));
        assert!(error.to_string().len() < 128);
    }

    #[tokio::test]
    async fn normal_soap_fault_body_is_returned_for_structured_parsing() {
        let fault = b"<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value></s:Code></s:Fault></s:Body></s:Envelope>";
        let endpoint = serve_once(
            500,
            &format!("Content-Length: {}\r\n", fault.len()),
            fault.to_vec(),
        )
        .await;
        let body = SoapTransport::new(&endpoint)
            .unwrap()
            .soap_post(&endpoint.base_url(), "urn:test", "<request/>".to_owned())
            .await
            .unwrap();
        assert_eq!(body.as_bytes(), fault);
    }

    #[tokio::test]
    async fn authenticated_soap_does_not_follow_redirects() {
        for status in [307, 308] {
            let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let sink_address = sink.local_addr().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8_192];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Redirect\r\nLocation: http://{sink_address}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let endpoint = endpoint_for(address);
            let error = SoapTransport::new(&endpoint)
                .unwrap()
                .soap_post(
                    &endpoint.base_url(),
                    "urn:test",
                    "<PasswordDigest>marker</PasswordDigest>".to_owned(),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(&status.to_string()));
            assert!(
                tokio::time::timeout(Duration::from_millis(200), sink.accept())
                    .await
                    .is_err(),
                "redirect target received authenticated request for {status}"
            );
            server.await.unwrap();
        }
    }
}
