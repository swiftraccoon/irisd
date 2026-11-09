//! A loopback ONVIF camera for tests: one canned SOAP reply per connection,
//! each request reported as its path and lowercase `Content-Type` line.

use std::fmt::Write as _;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::endpoint::CameraEndpoint;

/// The endpoint of a loopback camera at `host:port`, with marker credentials.
pub(crate) fn endpoint(host: &str, port: u16) -> CameraEndpoint {
    CameraEndpoint {
        camera: "cam1".parse().expect("a valid camera id"),
        host: host.to_owned(),
        http_port: port,
        https: false,
        tls_verify: true,
        username: "marker-user".to_owned(),
        password: "marker-password".to_owned(),
    }
}

/// Serves one SOAP request per connection until the listener fails; reports
/// `(path, action)` of each. Service URLs in the replies start with
/// `advertised`.
pub(crate) async fn camera(
    listener: TcpListener,
    advertised: String,
    requests: UnboundedSender<(String, String)>,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let mut complete = None;
        while complete.is_none() {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&chunk[..read]),
            }
            let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..end + 4]).to_ascii_lowercase();
            let body_len: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap_or(0));
            if request.len() >= end + 4 + body_len {
                let body = String::from_utf8_lossy(&request[end + 4..end + 4 + body_len])
                    .to_ascii_lowercase();
                complete = Some((head, body));
            }
        }
        // A connection that ends before a complete request is not a call.
        let Some((head, body)) = complete else {
            continue;
        };
        let path = head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let action = head
            .lines()
            .find(|line| line.starts_with("content-type:"))
            .unwrap_or_default()
            .to_owned();
        let reply = reply(&action, &body, &advertised);
        let _ = requests.send((path, action));
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            )
            .await
            .expect("the loopback camera writes its reply");
    }
}

/// The canned reply to one SOAP action, given as the lowercase text that
/// names it (the request's `Content-Type` line or the SOAP action URN) and the
/// lowercase request body.
///
/// # Panics
///
/// Panics on an action the fixture does not know.
pub(crate) fn reply(action: &str, body: &str, advertised: &str) -> String {
    if action.contains("getstreamuri") {
        let token = body
            .split("<trt:profiletoken>")
            .nth(1)
            .and_then(|rest| rest.split("</trt:profiletoken>").next())
            .unwrap_or("none");
        return stream_uri_reply(&format!("rtsp://203.0.113.9:8554/live/{token}"));
    }
    if action.contains("getsystemdateandtime") {
        return r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>0</tt:Hour><tt:Minute>0</tt:Minute><tt:Second>0</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>1</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>"#.to_owned();
    }
    if action.contains("getcapabilities") {
        return format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Media><tt:XAddr>{advertised}/onvif/media</tt:XAddr></tt:Media><tt:Events><tt:XAddr>{advertised}/onvif/events</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#
        );
    }
    if action.contains("createpullpointsubscriptionrequest") {
        return format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing"><s:Body><tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa:Address>{advertised}/onvif/events/subscription</wsa:Address></tev:SubscriptionReference><tev:CurrentTime>2026-01-01T00:00:00Z</tev:CurrentTime><tev:TerminationTime>2026-01-01T00:10:00Z</tev:TerminationTime></tev:CreatePullPointSubscriptionResponse></s:Body></s:Envelope>"#
        );
    }
    if action.contains("getprofiles") {
        return profiles_reply(&[("profile-1", "Main"), ("profile-2", "Sub")]);
    }
    if action.contains("getsnapshoturi") {
        return snapshot_uri_reply(&format!("{advertised}/snap.jpg?channel=1"));
    }
    if action.contains("pullmessagesrequest") {
        return r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><PullMessagesResponse/></s:Body></s:Envelope>"#.to_owned();
    }
    if action.contains("unsubscriberequest") {
        return r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><UnsubscribeResponse/></s:Body></s:Envelope>"#.to_owned();
    }
    panic!("unexpected action {action}");
}

/// A `GetProfilesResponse` listing `(token, name)` profiles.
pub(crate) fn profiles_reply(profiles: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (token, name) in profiles {
        let _ = write!(
            body,
            r#"<trt:Profiles token="{token}" fixed="true"><tt:Name>{name}</tt:Name></trt:Profiles>"#
        );
    }
    format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><trt:GetProfilesResponse>{body}</trt:GetProfilesResponse></s:Body></s:Envelope>"#
    )
}

/// A `GetStreamUriResponse` naming `uri`.
pub(crate) fn stream_uri_reply(uri: &str) -> String {
    format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>{uri}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetStreamUriResponse></s:Body></s:Envelope>"#
    )
}

/// A `GetSnapshotUriResponse` naming `uri`.
pub(crate) fn snapshot_uri_reply(uri: &str) -> String {
    format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><trt:GetSnapshotUriResponse><trt:MediaUri><tt:Uri>{uri}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetSnapshotUriResponse></s:Body></s:Envelope>"#
    )
}

/// A SOAP fault whose code, subcode, and reason mark an expired WS-Security
/// timestamp.
pub(crate) fn clock_fault_reply() -> String {
    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>wsse:MessageExpired</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Security timestamp expired</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#.to_owned()
}
