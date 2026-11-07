//! `onvif-call`: one ONVIF SOAP call with a WS-UsernameToken password
//! digest, compensating for the camera's clock offset.

use anyhow::Context;
use base64::Engine as _;
use clap::Parser;
use sha1::{Digest, Sha1};

pub use iris_tools::probe_parser::camera_utc;

use crate::logs::terminal_safe;
use crate::time::{iso, now};

const MAX_RESPONSE: usize = 4 * 1024 * 1024;

fn bounded_offset(value: &str) -> Result<f64, String> {
    let value = value.parse::<f64>().map_err(|error| error.to_string())?;
    if value.is_finite() && (-86_400.0..=86_400.0).contains(&value) {
        Ok(value)
    } else {
        Err("offset must be finite and between -86400 and 86400 seconds".into())
    }
}

fn bounded_limit(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|error| error.to_string())?;
    if (1..=MAX_RESPONSE).contains(&value) {
        Ok(value)
    } else {
        Err(format!("limit must be between 1 and {MAX_RESPONSE}"))
    }
}

/// Arguments of `onvif-call`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The ONVIF user.
    user: String,
    /// The password (an argument so nothing secret is stored in a file).
    password: String,
    /// The service URL, for example `http://192.0.2.10/onvif/event_service`.
    service_url: String,
    /// The SOAP body's inner XML.
    body_xml: String,
    /// Print at most this many bytes of the response.
    #[arg(value_parser = bounded_limit)]
    limit: usize,
    /// Camera clock minus host clock in seconds; `0` uses host time. Without
    /// it the offset is measured with an unauthenticated `GetSystemDateAndTime`.
    #[arg(value_parser = bounded_offset)]
    offset_seconds: Option<f64>,
}

fn xml_text(value: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        value.chars().all(|c| {
            matches!(c, '\t' | '\n' | '\r')
                || (!c.is_control() && c != '\u{fffe}' && c != '\u{ffff}')
        }),
        "XML text contains a forbidden control character"
    );
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

/// The WS-Security header for `user` at `created` (RFC 3339) with `nonce`.
pub fn security_header(
    user: &str,
    password: &str,
    nonce: &[u8],
    created: &str,
) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    let digest = b64.encode(hasher.finalize());
    let user = xml_text(user)?;
    Ok(format!(
        "<s:Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\" s:mustUnderstand=\"1\"><UsernameToken>\
<Username>{user}</Username><Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
<Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{}</Nonce>\
<Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">{created}</Created></UsernameToken></Security></s:Header>",
        b64.encode(nonce)
    ))
}

async fn bounded_text(response: reqwest::Response, limit: usize) -> anyhow::Result<String> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= limit as u64,
            "ONVIF response exceeds {limit} bytes"
        );
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes
                .len()
                .checked_add(chunk.len())
                .is_some_and(|size| size <= limit),
            "ONVIF response exceeds {limit} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("ONVIF response is not UTF-8")
}

async fn call(args: &Args) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let service = reqwest::Url::parse(&args.service_url).context("parsing service URL")?;
    anyhow::ensure!(
        matches!(service.scheme(), "http" | "https"),
        "service URL must use HTTP or HTTPS"
    );
    anyhow::ensure!(
        service.username().is_empty() && service.password().is_none(),
        "service URL cannot contain userinfo"
    );
    let offset = if let Some(o) = args.offset_seconds {
        o
    } else {
        {
            let mut device = service.clone();
            let prefix = service
                .path()
                .find("/onvif/")
                .map_or("", |index| &service.path()[..index]);
            device.set_path(&format!("{prefix}/onvif/device_service"));
            device.set_query(None);
            device.set_fragment(None);
            let query = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/></s:Body></s:Envelope>";
            let response = client
                .post(device)
                .header("Content-Type", "application/soap+xml")
                .body(query)
                .send()
                .await
                .context("GetSystemDateAndTime")?;
            let status = response.status();
            let text = bounded_text(response, 256 * 1024).await?;
            anyhow::ensure!(
                status.is_success(),
                "GetSystemDateAndTime returned HTTP {}: {}",
                status.as_u16(),
                terminal_safe(&text.chars().take(500).collect::<String>())
            );
            camera_utc(&text).context("no UTC time in the GetSystemDateAndTime response")? - now()
        }
    };
    eprintln!("[camera UTC minus host UTC: {offset:+.0}s]");
    let mut nonce = [0u8; 16];
    getrandom(&mut nonce)?;
    let created = iso(now() + offset).replace('Z', ".000Z");
    let header = security_header(&args.user, &args.password, &nonce, &created)?;
    let envelope = format!(
        "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">{header}<s:Body>{}</s:Body></s:Envelope>",
        args.body_xml
    );
    let response = client
        .post(service)
        .header("Content-Type", "application/soap+xml; charset=utf-8")
        .body(envelope)
        .send()
        .await?;
    let status = response.status();
    let text = bounded_text(response, args.limit).await?;
    anyhow::ensure!(
        status.is_success(),
        "ONVIF call returned HTTP {}: {}",
        status.as_u16(),
        terminal_safe(&text.chars().take(1500).collect::<String>())
    );
    println!("{}", terminal_safe(&text));
    Ok(())
}

fn getrandom(buf: &mut [u8]) -> anyhow::Result<()> {
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)?;
    Ok(())
}

/// Runs the tool.
pub fn run(args: &Args) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(call(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_camera_time_and_builds_the_digest() {
        let response = "<tt:UTCDateTime><tt:Time><tt:Hour>1</tt:Hour><tt:Minute>2</tt:Minute><tt:Second>3</tt:Second></tt:Time><tt:Date><tt:Year>1970</tt:Year><tt:Month>1</tt:Month><tt:Day>2</tt:Day></tt:Date></tt:UTCDateTime>";
        assert_eq!(camera_utc(response), Some(86_400.0 + 3723.0));
        let header =
            security_header("u", "p", b"0123456789abcdef", "1970-01-02T01:02:03.000Z").unwrap();
        assert!(header.contains("<Username>u</Username>"));
        assert!(header.contains("<Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">MDEyMzQ1Njc4OWFiY2RlZg==</Nonce>"));
        assert!(header.contains("#PasswordDigest\">"));
        assert!(
            security_header("<&", "p", b"0123456789abcdef", "1970-01-02T01:02:03.000Z")
                .unwrap()
                .contains("<Username>&lt;&amp;</Username>")
        );
        assert!(
            security_header(
                "bad\u{1}",
                "p",
                b"0123456789abcdef",
                "1970-01-02T01:02:03.000Z"
            )
            .is_err()
        );
        assert_eq!(
            camera_utc(
                "<tt:UTCDateTime><tt:Time><tt:Hour>999999999999999999</tt:Hour><tt:Minute>0</tt:Minute><tt:Second>0</tt:Second></tt:Time><tt:Date><tt:Year>999999999999999999</tt:Year><tt:Month>1</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime>"
            ),
            None
        );
        assert_eq!(
            camera_utc(
                "<tt:UTCDateTime><tt:Time><tt:Hour>0</tt:Hour><tt:Minute>0</tt:Minute><tt:Second>0</tt:Second></tt:Time><tt:Date><tt:Year>9223372036854775807</tt:Year><tt:Month>1</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime>"
            ),
            None
        );
    }
}
