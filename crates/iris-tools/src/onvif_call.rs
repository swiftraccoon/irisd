//! `onvif-call`: one ONVIF SOAP call with a WS-UsernameToken password
//! digest, compensating for the camera's clock offset. The call is a read or
//! an event-subscription operation; anything that could change the camera's
//! configuration is refused before a request is sent.

use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use clap::Parser;
use reqwest::redirect::Policy;
use reqwest::{Client, Response, Url};
use sha1::{Digest, Sha1};

use crate::logs;
use crate::probe_parser;
use crate::secret::{Password, password_from_env};
use crate::time;

/// Largest response LIMIT may allow.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
/// Largest `GetSystemDateAndTime` response read for the clock offset.
const MAX_CLOCK_RESPONSE: usize = 256 * 1024;
/// Characters of a failed response's body shown in the error.
const ERROR_BODY_CHARS: usize = 1_500;
/// The operations allowed beside every `Get…` read: the PullPoint event
/// subscription, whose subscriptions expire and change no configuration.
const SUBSCRIPTION_OPERATIONS: [&str; 4] = [
    "CreatePullPointSubscription",
    "PullMessages",
    "Renew",
    "Unsubscribe",
];

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

/// Arguments of `onvif-call`. `Debug` redacts the password.
#[derive(Debug, Parser)]
pub struct Args {
    /// Environment variable holding the ONVIF password, which stays out of
    /// process listings and shell history.
    #[arg(long = "password-env", value_name = "VAR", value_parser = password_from_env)]
    password: Password,
    /// The ONVIF user.
    user: String,
    /// The service URL, for example `http://192.0.2.10/onvif/event_service`.
    service_url: String,
    /// The SOAP body's inner XML: one `Get…` operation, or one of
    /// `CreatePullPointSubscription`, `PullMessages`, `Renew`, `Unsubscribe`.
    body_xml: String,
    /// Print at most this many bytes of the response, from 1 to 4 MiB; a
    /// longer response is cut there and marked as cut.
    #[arg(value_parser = bounded_limit)]
    limit: usize,
    /// Camera clock minus host clock in seconds; `0` uses host time. Without
    /// it the offset is measured with an unauthenticated `GetSystemDateAndTime`
    /// request before the call.
    #[arg(value_parser = bounded_offset)]
    offset_seconds: Option<f64>,
    /// Accept the camera's certificate without verification, for an HTTPS-only
    /// camera with a self-signed certificate (the daemon's `tls_verify = false`).
    #[arg(long = "no-tls-verify")]
    no_tls_verify: bool,
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

/// The index of the `>` that ends the tag `text` starts, outside quoted
/// attribute values.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    text.char_indices().find_map(|(index, c)| {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '>') => return Some(index),
            _ => {}
        }
        None
    })
}

/// The local name of the one top-level element of a SOAP body: whitespace
/// may surround it, nothing else may, and comments, processing
/// instructions, CDATA sections, and declarations are refused.
fn operation(body: &str) -> anyhow::Result<&str> {
    let mut depth = 0usize;
    let mut first = None;
    let mut rest = body;
    while let Some(open) = rest.find('<') {
        anyhow::ensure!(
            depth > 0 || rest[..open].trim().is_empty(),
            "BODY_XML holds text outside its element"
        );
        let after = &rest[open + 1..];
        let end = tag_end(after).context("BODY_XML has an unterminated tag")?;
        let tag = &after[..end];
        rest = &after[end + 1..];
        if tag.starts_with(['!', '?']) {
            anyhow::bail!(
                "BODY_XML may not hold comments, processing instructions, CDATA, or declarations"
            );
        }
        if tag.starts_with('/') {
            depth = depth
                .checked_sub(1)
                .context("BODY_XML closes an element it did not open")?;
            continue;
        }
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        anyhow::ensure!(!name.is_empty(), "BODY_XML has an element without a name");
        if depth == 0 {
            anyhow::ensure!(first.is_none(), "BODY_XML must hold exactly one operation");
            first = Some(name.rsplit(':').next().unwrap_or(name));
        }
        if !tag.ends_with('/') {
            depth += 1;
        }
    }
    anyhow::ensure!(
        depth == 0 && rest.trim().is_empty(),
        "BODY_XML leaves an element open or holds text after it"
    );
    first.context("BODY_XML holds no operation")
}

/// Refuses a body whose operation could change the camera.
fn check_read_only(body: &str) -> anyhow::Result<()> {
    let operation = operation(body)?;
    anyhow::ensure!(
        operation.starts_with("Get") || SUBSCRIPTION_OPERATIONS.contains(&operation),
        "{operation} is not a Get operation or a PullPoint subscription operation; onvif-call \
         never changes a camera"
    );
    Ok(())
}

/// The WS-Security header for `user` at `created` (RFC 3339) with `nonce`:
/// the password digest is Base64(SHA-1(nonce ‖ created ‖ password)).
fn security_header(
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
        "<s:Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/\
         oasis-200401-wss-wssecurity-secext-1.0.xsd\" s:mustUnderstand=\"1\"><UsernameToken>\
         <Username>{user}</Username><Password Type=\"http://docs.oasis-open.org/wss/2004/01/\
         oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
         <Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/\
         oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{}</Nonce>\
         <Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/\
         oasis-200401-wss-wssecurity-utility-1.0.xsd\">{created}</Created></UsernameToken>\
         </Security></s:Header>",
        b64.encode(nonce)
    ))
}

/// Reads at most `limit` bytes of a response body; the flag tells whether
/// more followed.
async fn read_prefix(mut response: Response, limit: usize) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = limit - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

/// A body read in full, which must fit in `limit` bytes and be UTF-8.
async fn bounded_text(response: Response, limit: usize) -> anyhow::Result<String> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= limit as u64,
            "ONVIF response exceeds {limit} bytes"
        );
    }
    let (bytes, cut) = read_prefix(response, limit).await?;
    anyhow::ensure!(!cut, "ONVIF response exceeds {limit} bytes");
    String::from_utf8(bytes).context("ONVIF response is not UTF-8")
}

/// The UTF-8 text of a response prefix; when the prefix was cut, a character
/// the cut split is dropped.
fn prefix_text(bytes: &[u8], cut: bool) -> anyhow::Result<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) if cut && error.error_len().is_none() => {
            Ok(std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default())
        }
        Err(error) => Err(error).context("ONVIF response is not UTF-8"),
    }
}

/// The camera's clock minus the host's, from an unauthenticated
/// `GetSystemDateAndTime` to the device service beside `service`.
async fn camera_offset(client: &Client, service: &Url) -> anyhow::Result<f64> {
    let mut device = service.clone();
    let prefix = service
        .path()
        .find("/onvif/")
        .map_or("", |index| &service.path()[..index]);
    device.set_path(&format!("{prefix}/onvif/device_service"));
    device.set_query(None);
    device.set_fragment(None);
    let query = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>\
                 <GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>\
                 </s:Body></s:Envelope>";
    let response = client
        .post(device)
        .header("Content-Type", "application/soap+xml")
        .body(query)
        .send()
        .await
        .context("GetSystemDateAndTime")?;
    let status = response.status();
    let text = bounded_text(response, MAX_CLOCK_RESPONSE).await?;
    anyhow::ensure!(
        status.is_success(),
        "GetSystemDateAndTime returned HTTP {}: {}",
        status.as_u16(),
        logs::terminal_safe(&text.chars().take(500).collect::<String>())
    );
    Ok(probe_parser::camera_utc(&text)
        .context("no UTC time in the GetSystemDateAndTime response")?
        - time::now())
}

async fn call(args: &Args) -> anyhow::Result<()> {
    check_read_only(&args.body_xml)?;
    let client = Client::builder()
        .danger_accept_invalid_certs(args.no_tls_verify)
        .timeout(Duration::from_secs(15))
        .redirect(Policy::none())
        .build()?;
    let service = Url::parse(&args.service_url).context("parsing service URL")?;
    anyhow::ensure!(
        matches!(service.scheme(), "http" | "https"),
        "service URL must use HTTP or HTTPS"
    );
    anyhow::ensure!(
        service.username().is_empty() && service.password().is_none(),
        "service URL cannot contain userinfo"
    );
    let offset = match args.offset_seconds {
        Some(offset) => offset,
        None => camera_offset(&client, &service).await?,
    };
    eprintln!("[camera UTC minus host UTC: {offset:+.0}s]");
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).context("drawing the WS-Security nonce")?;
    let created = time::iso(time::now() + offset).replace('Z', ".000Z");
    let header = security_header(&args.user, args.password.expose(), &nonce, &created)?;
    let envelope = format!(
        "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">{header}<s:Body>{}\
         </s:Body></s:Envelope>",
        args.body_xml
    );
    let response = client
        .post(service)
        .header("Content-Type", "application/soap+xml; charset=utf-8")
        .body(envelope)
        .send()
        .await?;
    let status = response.status();
    let (bytes, cut) = read_prefix(response, args.limit).await?;
    let text = prefix_text(&bytes, cut)?;
    anyhow::ensure!(
        status.is_success(),
        "ONVIF call returned HTTP {}: {}",
        status.as_u16(),
        logs::terminal_safe(&text.chars().take(ERROR_BODY_CHARS).collect::<String>())
    );
    println!("{}", logs::terminal_safe(text));
    if cut {
        eprintln!("[response cut at {} bytes]", args.limit);
    }
    Ok(())
}

/// Checks that the body is a read or subscription operation, measures the
/// camera's clock offset unless one is given, sends the call, and prints at
/// most LIMIT bytes of the response, escaped for the terminal.
pub fn run(args: &Args) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(call(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATED: &str = "1970-01-02T01:02:03.000Z";

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(
            [
                "onvif-call",
                "--password-env",
                "PATH",
                "user",
                "http://192.0.2.10/onvif/event_service",
                "<GetEventProperties/>",
                "1000",
            ]
            .iter()
            .chain(extra),
        )
    }

    #[test]
    fn the_password_comes_from_the_environment_and_stays_out_of_debug() {
        let expected = std::env::var("PATH").expect("tests run with PATH set");
        let args = parse(&[]).unwrap();
        assert_eq!(args.password.expose(), expected);
        assert_eq!(args.user, "user");
        assert!(!format!("{args:?}").contains(&expected));
    }

    #[test]
    fn an_unset_password_variable_is_named() {
        let error = Args::try_parse_from([
            "onvif-call",
            "--password-env",
            "IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A",
            "user",
            "http://192.0.2.10/onvif/event_service",
            "<GetEventProperties/>",
            "1000",
        ])
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A") && error.contains("not set"),
            "{error}"
        );
    }

    #[test]
    fn certificates_are_verified_unless_asked_otherwise() {
        assert!(!parse(&[]).unwrap().no_tls_verify);
        assert!(parse(&["--no-tls-verify"]).unwrap().no_tls_verify);
    }

    #[test]
    fn the_digest_hashes_the_nonce_the_creation_time_and_the_password() {
        // Base64(SHA-1("0123456789abcdef" ‖ CREATED ‖ "p")), computed with sha1sum and base64.
        let header = security_header("u", "p", b"0123456789abcdef", CREATED).unwrap();
        assert!(
            header.contains("#PasswordDigest\">6faE5yU+nHiu/estcytNytQxeo4=</Password>"),
            "{header}"
        );
    }

    #[test]
    fn the_header_carries_the_user_and_the_encoded_nonce() {
        let header = security_header("u", "p", b"0123456789abcdef", CREATED).unwrap();
        assert!(header.contains("<Username>u</Username>"));
        assert!(header.contains("#Base64Binary\">MDEyMzQ1Njc4OWFiY2RlZg==</Nonce>"));
        assert!(header.contains(&format!(">{CREATED}</Created>")));
    }

    #[test]
    fn the_user_is_escaped_and_controls_are_refused() {
        assert!(
            security_header("<&", "p", b"0123456789abcdef", CREATED)
                .unwrap()
                .contains("<Username>&lt;&amp;</Username>")
        );
        assert!(security_header("bad\u{1}", "p", b"0123456789abcdef", CREATED).is_err());
    }

    #[test]
    fn reads_and_subscriptions_are_allowed() {
        for body in [
            "<GetEventProperties xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
            " <tds:GetSystemDateAndTime/>\n",
            "<tev:PullMessages><tev:Timeout>PT5S</tev:Timeout>\
             <tev:MessageLimit>10</tev:MessageLimit></tev:PullMessages>",
            "<CreatePullPointSubscription attr=\"a>b\"/>",
        ] {
            check_read_only(body).unwrap_or_else(|error| panic!("{body}: {error:#}"));
        }
    }

    #[test]
    fn operations_that_change_a_camera_are_refused() {
        for body in [
            "<tds:SystemReboot/>",
            "<SetSystemDateAndTime><DateTimeType>Manual</DateTimeType></SetSystemDateAndTime>",
            "<tds:GetUsers/><tds:SetUser/>",
            "<GetUsers/></s:Body><s:Body><SystemReboot/>",
            "<!-- --><SystemReboot/>",
            "<GetUsers><![CDATA[x]]></GetUsers>",
            "text<GetUsers/>",
            "<GetUsers>",
            "",
        ] {
            assert!(check_read_only(body).is_err(), "{body}");
        }
    }

    #[test]
    fn a_cut_prefix_drops_only_a_split_character() {
        let bytes = "aé".as_bytes();
        assert_eq!(prefix_text(&bytes[..2], true).unwrap(), "a");
        assert!(prefix_text(&bytes[..2], false).is_err());
        assert!(prefix_text(b"a\xffb", true).is_err());
    }
}
