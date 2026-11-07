use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oxvif::OnvifClient;
use oxvif::transport::{Transport, TransportError};

const USER_MARKER: &str = "trace-user-marker";
const PASSWORD_MARKER: &str = "trace-password-marker";
const RESPONSE_MARKER: &str = "trace-response-marker";

#[derive(Clone)]
struct TraceCapture {
    output: Arc<Mutex<String>>,
}

impl tracing::Subscriber for TraceCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor<'a>(&'a mut String);

        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = write!(self.0, "{}={value:?};", field.name());
            }
        }

        let mut output = self.output.lock().expect("trace capture lock");
        event.record(&mut Visitor(&mut output));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

struct RecordingTransport {
    request: Arc<Mutex<String>>,
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn soap_post(
        &self,
        _url: &str,
        _action: &str,
        body: String,
    ) -> Result<String, TransportError> {
        *self.request.lock().expect("request capture lock") = body;
        Ok(format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Events><tt:XAddr>http://{RESPONSE_MARKER}/events</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#
        ))
    }
}

fn element_text<'a>(xml: &'a str, open: &str, close: &str) -> &'a str {
    let start = xml.find(open).expect("request contains marker element");
    let content = &xml[start..];
    let start = content.find('>').expect("marker open tag closes") + 1;
    let content = &content[start..];
    let end = content.find(close).expect("marker close tag exists");
    &content[..end]
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_trace_excludes_request_and_response_bodies() {
    let output = Arc::new(Mutex::new(String::new()));
    let request = Arc::new(Mutex::new(String::new()));
    let subscriber = TraceCapture {
        output: output.clone(),
    };
    let transport = Arc::new(RecordingTransport {
        request: request.clone(),
    });
    let client = OnvifClient::new("http://192.0.2.1/onvif/device_service")
        .with_credentials(USER_MARKER, PASSWORD_MARKER)
        .with_transport(transport);
    let _guard = tracing::subscriber::set_default(subscriber);

    let capabilities = client.get_capabilities().await.expect("mock call succeeds");
    assert_eq!(
        capabilities.events.url.as_deref(),
        Some("http://trace-response-marker/events")
    );

    let request = request.lock().expect("request capture lock");
    let password_digest = element_text(&request, "<wsse:Password", "</wsse:Password>");
    let nonce = element_text(&request, "<wsse:Nonce", "</wsse:Nonce>");
    let created = element_text(&request, "<wsu:Created", "</wsu:Created>");
    assert!(!password_digest.is_empty());
    assert!(!nonce.is_empty());
    assert!(!created.is_empty());

    let output = output.lock().expect("trace capture lock");
    assert!(output.contains("GetCapabilities"));
    for marker in [
        USER_MARKER,
        PASSWORD_MARKER,
        RESPONSE_MARKER,
        password_digest,
        nonce,
        created,
        "PasswordDigest",
        "<wsse:Nonce",
        "<wsu:Created",
    ] {
        assert!(!output.contains(marker), "trace leaked marker {marker}");
    }
}
