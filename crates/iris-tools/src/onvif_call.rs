//! `onvif-call`: one ONVIF SOAP call with a WS-UsernameToken password
//! digest, compensating for the camera's clock offset.

use anyhow::Context;
use base64::Engine as _;
use clap::Parser;
use sha1::{Digest, Sha1};

use crate::time::{days_from_civil, iso, now};

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
    limit: usize,
    /// Camera clock minus host clock in seconds; `0` uses host time. Without
    /// it the offset is measured with an unauthenticated `GetSystemDateAndTime`.
    offset_seconds: Option<f64>,
}

fn tag(text: &str, name: &str) -> Option<i64> {
    let open = format!("<tt:{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find('<')? + start;
    text[start..end].trim().parse().ok()
}

/// The camera's UTC time from a `GetSystemDateAndTime` response, as seconds.
pub fn camera_utc(response: &str) -> Option<f64> {
    let utc_start = response.find("<tt:UTCDateTime>")?;
    let utc = &response[utc_start..];
    let (h, mi, s) = (tag(utc, "Hour")?, tag(utc, "Minute")?, tag(utc, "Second")?);
    let (y, mo, d) = (tag(utc, "Year")?, tag(utc, "Month")?, tag(utc, "Day")?);
    Some((days_from_civil(y, mo, d)? * 86_400 + h * 3600 + mi * 60 + s) as f64)
}

/// The WS-Security header for `user` at `created` (RFC 3339) with `nonce`.
pub fn security_header(user: &str, password: &str, nonce: &[u8], created: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    let digest = b64.encode(hasher.finalize());
    format!(
        "<s:Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\" s:mustUnderstand=\"1\"><UsernameToken>\
<Username>{user}</Username><Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
<Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{}</Nonce>\
<Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">{created}</Created></UsernameToken></Security></s:Header>",
        b64.encode(nonce)
    )
}

async fn call(args: &Args) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let offset = if let Some(o) = args.offset_seconds {
        o
    } else {
        {
            let device = match args.service_url.find("/onvif/") {
                Some(i) => format!("{}/onvif/device_service", &args.service_url[..i]),
                None => args.service_url.clone(),
            };
            let query = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/></s:Body></s:Envelope>";
            let text = client
                .post(&device)
                .header("Content-Type", "application/soap+xml")
                .body(query)
                .send()
                .await
                .context("GetSystemDateAndTime")?
                .text()
                .await?;
            camera_utc(&text).context("no UTC time in the GetSystemDateAndTime response")? - now()
        }
    };
    eprintln!("[camera UTC minus host UTC: {offset:+.0}s]");
    let mut nonce = [0u8; 16];
    getrandom(&mut nonce)?;
    let created = iso(now() + offset).replace('Z', ".000Z");
    let header = security_header(&args.user, &args.password, &nonce, &created);
    let envelope = format!(
        "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">{header}<s:Body>{}</s:Body></s:Envelope>",
        args.body_xml
    );
    let response = client
        .post(&args.service_url)
        .header("Content-Type", "application/soap+xml; charset=utf-8")
        .body(envelope)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    let out = if status.is_success() {
        text
    } else {
        format!(
            "HTTP {}: {}",
            status.as_u16(),
            text.chars().take(1500).collect::<String>()
        )
    };
    println!("{}", out.chars().take(args.limit).collect::<String>());
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
        let header = security_header("u", "p", b"0123456789abcdef", "1970-01-02T01:02:03.000Z");
        assert!(header.contains("<Username>u</Username>"));
        assert!(header.contains("<Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">MDEyMzQ1Njc4OWFiY2RlZg==</Nonce>"));
        assert!(header.contains("#PasswordDigest\">"));
    }
}
