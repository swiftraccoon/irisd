//! WS-Discovery of ONVIF cameras, for the Add camera flow.
//!
//! [`discover`] sends one WS-Discovery `Probe` through `oxvif` (UDP multicast
//! to `239.255.255.250:3702` from every IPv4 interface) and maps the answers to
//! [`DiscoveredCamera`]s: the host and port of the first usable device service
//! URL, and the name, hardware, and location scopes, percent-decoded, stripped
//! of control characters, and bounded. Everything a device sends is untrusted:
//! nothing here follows a URL, and the daemon's later calls go only to the host
//! an operator confirmed.

use std::io;
use std::time::Duration;

use iris_core::config::MAX_HOST_BYTES;
use oxvif::discovery::DiscoveredDevice;
use tracing::debug;
use url::Url;

use crate::text;

pub use crate::text::MAX_TEXT_CHARS;

/// Most cameras one probe lists; once that many distinct hosts and ports are
/// known, further answers are ignored.
pub const MAX_DISCOVERED: usize = 64;
/// Longest endpoint reference kept.
const MAX_ENDPOINT_CHARS: usize = 256;
/// Scopes and service URLs looked at per device.
pub const MAX_LIST_ENTRIES: usize = 16;
const SCOPE_PREFIX: &str = "onvif://www.onvif.org/";

/// A camera WS-Discovery found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredCamera {
    /// The device's endpoint reference, usually a `urn:uuid:` name.
    pub endpoint: String,
    /// The host of its device service, a bare name or address.
    pub host: String,
    /// The port of its device service.
    pub port: u16,
    /// Whether the device service is reached over HTTPS.
    pub https: bool,
    /// The `name` scope, when the device sends one.
    pub name: Option<String>,
    /// The `hardware` scope (a model string), when sent.
    pub hardware: Option<String>,
    /// The `location` scope, when sent.
    pub location: Option<String>,
}

impl DiscoveredCamera {
    /// Maps one probe answer; `None` when none of its first
    /// [`MAX_LIST_ENTRIES`] device service URLs is a usable `http` or `https`
    /// URL without userinfo.
    #[must_use]
    pub fn from_device(device: &DiscoveredDevice) -> Option<Self> {
        Self::from_parts(&device.endpoint, &device.scopes, &device.xaddrs)
    }

    /// Maps the parts of a probe answer: the endpoint reference, the scopes,
    /// and the device service URLs. Accepts anything a network peer could
    /// send; only the first [`MAX_LIST_ENTRIES`] scopes and the first
    /// [`MAX_LIST_ENTRIES`] service URLs are examined, and every text is cut
    /// to [`MAX_TEXT_CHARS`] characters (the endpoint reference to 256).
    ///
    /// ```
    /// use iris_events::DiscoveredCamera;
    ///
    /// let camera = DiscoveredCamera::from_parts(
    ///     "urn:uuid:7",
    ///     &["onvif://www.onvif.org/name/Front%20Door".to_owned()],
    ///     &["http://192.0.2.5:8080/onvif/device_service".to_owned()],
    /// )
    /// .expect("a usable device service URL");
    /// assert_eq!((camera.host.as_str(), camera.port), ("192.0.2.5", 8080));
    /// assert_eq!(camera.name.as_deref(), Some("Front Door"));
    /// ```
    #[must_use]
    pub fn from_parts(endpoint: &str, scopes: &[String], xaddrs: &[String]) -> Option<Self> {
        let (host, port, https) = xaddrs
            .iter()
            .take(MAX_LIST_ENTRIES)
            .find_map(|xaddr| service_host(xaddr))?;
        let scope = |key: &str| {
            scopes
                .iter()
                .take(MAX_LIST_ENTRIES)
                .find_map(|scope| scope_value(scope, key))
                .and_then(|value| text::clean_text(&percent_decode(value), MAX_TEXT_CHARS))
        };
        Some(Self {
            endpoint: text::clean_text(endpoint, MAX_ENDPOINT_CHARS).unwrap_or_default(),
            host,
            port,
            https,
            name: scope("name"),
            hardware: scope("hardware"),
            location: scope("location"),
        })
    }
}

/// Probes the network for `wait` and lists the cameras that answered, one per
/// device service host and port, at most [`MAX_DISCOVERED`], sorted by host.
/// An error means the probe could not be sent at all; a quiet network is an
/// empty list. The probe itself keeps a bounded number of answer datagrams
/// (`oxvif::discovery::MAX_PROBE_DATAGRAMS`, 4 MiB in all).
///
/// # Errors
///
/// The error of binding or sending on the probe sockets.
pub async fn discover(wait: Duration) -> io::Result<Vec<DiscoveredCamera>> {
    let devices = oxvif::discovery::probe_result(wait).await?;
    Ok(list_cameras(&devices))
}

/// The distinct usable cameras among `devices`, the first [`MAX_DISCOVERED`]
/// by host and port in answer order, sorted by host.
fn list_cameras(devices: &[DiscoveredDevice]) -> Vec<DiscoveredCamera> {
    let mut cameras: Vec<DiscoveredCamera> = Vec::new();
    for device in devices {
        if cameras.len() == MAX_DISCOVERED {
            break;
        }
        let Some(camera) = DiscoveredCamera::from_device(device) else {
            continue;
        };
        if !cameras
            .iter()
            .any(|known| known.host == camera.host && known.port == camera.port)
        {
            cameras.push(camera);
        }
    }
    cameras.sort_by(|left, right| left.host.cmp(&right.host).then(left.port.cmp(&right.port)));
    debug!(
        answers = devices.len(),
        cameras = cameras.len(),
        "WS-Discovery probe finished"
    );
    cameras
}

/// The host, port, and scheme of a device service URL, when usable.
fn service_host(xaddr: &str) -> Option<(String, u16, bool)> {
    let url = Url::parse(xaddr).ok()?;
    let https = match url.scheme() {
        "http" => false,
        "https" => true,
        _ => return None,
    };
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = match url.host()? {
        url::Host::Domain(domain) => domain.to_owned(),
        url::Host::Ipv4(address) => address.to_string(),
        url::Host::Ipv6(address) => address.to_string(),
    };
    if host.is_empty() || host.len() > MAX_HOST_BYTES {
        return None;
    }
    Some((host, url.port_or_known_default()?, https))
}

/// The value of the `onvif://www.onvif.org/<key>/…` scope, still
/// percent-encoded.
fn scope_value<'a>(scope: &'a str, key: &str) -> Option<&'a str> {
    scope
        .strip_prefix(SCOPE_PREFIX)?
        .strip_prefix(key)?
        .strip_prefix('/')
}

/// Decodes `%XX` sequences; malformed ones stay as sent, and invalid UTF-8
/// becomes U+FFFD.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_probe_answer_maps_to_the_first_usable_service_and_decoded_scopes() {
        let camera = DiscoveredCamera::from_parts(
            "urn:uuid:0a1b",
            &strings(&[
                "onvif://www.onvif.org/type/video_encoder",
                "onvif://www.onvif.org/name/Front%20Door",
                "onvif://www.onvif.org/hardware/IPC-1",
                "onvif://www.onvif.org/location/country%2Fhall",
            ]),
            &strings(&[
                "http://user:pw@192.0.2.5/onvif/device_service",
                "ftp://192.0.2.5/",
                "http://192.0.2.5:8080/onvif/device_service",
                "http://192.0.2.6/onvif/device_service",
            ]),
        )
        .unwrap();
        assert_eq!(
            camera,
            DiscoveredCamera {
                endpoint: "urn:uuid:0a1b".into(),
                host: "192.0.2.5".into(),
                port: 8080,
                https: false,
                name: Some("Front Door".into()),
                hardware: Some("IPC-1".into()),
                location: Some("country/hall".into()),
            }
        );
    }

    #[test]
    fn https_and_ipv6_services_keep_a_bare_host_and_the_default_port() {
        let camera = DiscoveredCamera::from_parts(
            "",
            &[],
            &strings(&["https://[2001:db8::7]/onvif/device_service"]),
        )
        .unwrap();
        assert_eq!(
            (camera.host.as_str(), camera.port, camera.https),
            ("2001:db8::7", 443, true)
        );
        assert_eq!(camera.endpoint, "");
        assert_eq!(
            (camera.name, camera.hardware, camera.location),
            (None, None, None)
        );
    }

    #[test]
    fn unusable_services_make_no_camera() {
        assert!(
            DiscoveredCamera::from_parts(
                "x",
                &[],
                &strings(&[
                    "http://user@192.0.2.5/",
                    "rtsp://192.0.2.5/",
                    "http:///",
                    "nonsense"
                ]),
            )
            .is_none()
        );
    }

    #[test]
    fn percent_decoding_keeps_malformed_escapes() {
        assert_eq!(percent_decode("100%25%zz%ff"), "100%%zz\u{FFFD}");
    }

    #[test]
    fn only_the_first_sixteen_scopes_and_service_urls_are_examined() {
        let mut scopes = vec!["onvif://www.onvif.org/type/video_encoder".to_owned(); 16];
        scopes.push("onvif://www.onvif.org/name/Late".to_owned());
        let mut xaddrs = vec!["ftp://192.0.2.5/".to_owned(); 16];
        xaddrs.push("http://192.0.2.5/onvif/device_service".to_owned());
        assert!(DiscoveredCamera::from_parts("x", &scopes, &xaddrs).is_none());
        xaddrs[15] = "http://192.0.2.6/onvif/device_service".to_owned();
        let camera = DiscoveredCamera::from_parts("x", &scopes, &xaddrs).unwrap();
        assert_eq!((camera.host.as_str(), camera.name), ("192.0.2.6", None));
    }

    #[test]
    fn the_camera_list_is_bounded_by_distinct_cameras_not_answers() {
        let device = |host: &str, usable: bool| DiscoveredDevice {
            endpoint: format!("urn:uuid:{host}"),
            types: Vec::new(),
            scopes: Vec::new(),
            xaddrs: vec![if usable {
                format!("http://{host}/onvif/device_service")
            } else {
                format!("ftp://{host}/")
            }],
        };
        let mut devices: Vec<DiscoveredDevice> = (0..MAX_DISCOVERED)
            .map(|_| device("192.0.2.1", false))
            .collect();
        devices.extend((0..=MAX_DISCOVERED).map(|n| device(&format!("198.51.100.{n}"), true)));
        let cameras = list_cameras(&devices);
        assert_eq!(cameras.len(), MAX_DISCOVERED);
        assert!(
            cameras
                .iter()
                .all(|camera| camera.host.starts_with("198.51.100."))
        );
    }

    #[test]
    fn hostile_text_is_bounded() {
        let long_name = format!("onvif://www.onvif.org/name/{}", "a".repeat(500));
        let camera = DiscoveredCamera::from_parts(
            &"e".repeat(1000),
            &strings(&[
                long_name.as_str(),
                "onvif://www.onvif.org/hardware/line%0Abreak%07bell",
            ]),
            &strings(&["http://192.0.2.5/"]),
        )
        .unwrap();
        assert_eq!(camera.endpoint.chars().count(), MAX_ENDPOINT_CHARS);
        assert_eq!(camera.name.as_deref().map(str::len), Some(MAX_TEXT_CHARS));
        assert_eq!(camera.hardware.as_deref(), Some("linebreakbell"));
    }
}
