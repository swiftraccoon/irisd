//! Typed response structs for the ONVIF Events service.

use std::collections::HashMap;

use crate::error::OnvifError;
use crate::soap::xml::XmlContent;
use crate::soap::{SoapError, XmlNode};

/// Maximum bytes retained for one canonical [`ElementItem`] representation.
pub const MAX_ELEMENT_ITEM_BYTES: usize = 65_536;

// ── PullPointSubscription ─────────────────────────────────────────────────────

/// A pull-point event subscription returned by `CreatePullPointSubscription`.
///
/// Use `reference_url` as the endpoint for `pull_messages`,
/// `renew_subscription`, and `unsubscribe`.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct PullPointSubscription {
    /// Subscription manager endpoint URL.
    pub reference_url: String,
    /// ISO-8601 current device time at the moment of subscription.
    pub current_time: String,
    /// ISO-8601 timestamp when the subscription expires.
    pub termination_time: String,
}

impl PullPointSubscription {
    pub(crate) fn from_xml(resp: &XmlNode) -> Result<Self, OnvifError> {
        let reference_url = resp
            .path(&["SubscriptionReference", "Address"])
            .map(|n| n.text().to_string())
            .ok_or_else(|| SoapError::missing("SubscriptionReference/Address"))?;
        let termination_time = resp
            .child("TerminationTime")
            .map(|n| n.text().to_string())
            .unwrap_or_default();
        Ok(Self {
            reference_url,
            current_time: resp
                .child("CurrentTime")
                .map(|n| n.text().to_string())
                .unwrap_or_default(),
            termination_time,
        })
    }
}

/// Device clock and expiry returned after renewing a subscription.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionLease {
    /// ISO-8601 current device time at the moment of renewal.
    pub current_time: String,
    /// ISO-8601 timestamp when the renewed subscription expires.
    pub termination_time: String,
}

impl SubscriptionLease {
    pub(crate) fn from_xml(resp: &XmlNode) -> Self {
        Self {
            current_time: resp
                .child("CurrentTime")
                .map(|node| node.text().to_string())
                .unwrap_or_default(),
            termination_time: resp
                .child("TerminationTime")
                .map(|node| node.text().to_string())
                .unwrap_or_default(),
        }
    }
}

// ── PushSubscription ─────────────────────────────────────────────────────────

/// A WS-BaseNotification push subscription returned by `subscribe`.
///
/// Use `subscription_reference` as the endpoint for
/// [`renew_subscription`](crate::OnvifClient::renew_subscription) and
/// [`unsubscribe`](crate::OnvifClient::unsubscribe).
///
/// The device will POST `Notify` messages to the `consumer_url` you supplied
/// to [`subscribe`](crate::OnvifClient::subscribe). Use
/// [`notification_listener`](crate::notification_listener) to receive them.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct PushSubscription {
    /// Subscription manager endpoint URL — the device's subscription reference.
    pub subscription_reference: String,
    /// ISO-8601 current device time at the moment of subscription.
    pub current_time: String,
    /// ISO-8601 timestamp when the subscription expires.
    pub termination_time: String,
}

impl PushSubscription {
    pub(crate) fn from_xml(resp: &XmlNode) -> Result<Self, OnvifError> {
        let subscription_reference = resp
            .path(&["SubscriptionReference", "Address"])
            .map(|n| n.text().to_string())
            .ok_or_else(|| SoapError::missing("SubscriptionReference/Address"))?;
        Ok(Self {
            subscription_reference,
            current_time: resp
                .child("CurrentTime")
                .map(|n| n.text().to_string())
                .unwrap_or_default(),
            termination_time: resp
                .child("TerminationTime")
                .map(|n| n.text().to_string())
                .unwrap_or_default(),
        })
    }
}

// ── NotificationMessage ───────────────────────────────────────────────────────

/// A single ONVIF event notification received via PullMessages or push delivery.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct NotificationMessage {
    /// Topic path (e.g. `tns1:VideoSource/MotionAlarm`).
    pub topic: String,
    /// UTC timestamp from the `Message/@UtcTime` attribute.
    pub utc_time: String,
    /// `Message/@PropertyOperation` — typically `Initialized`, `Changed`,
    /// or `Deleted`. Empty when the device omits the attribute.
    pub property_operation: String,
    /// Source `SimpleItem` pairs (e.g. `VideoSourceToken = "VideoSource_1"`).
    pub source: HashMap<String, String>,
    /// Source `ElementItem` values in message order.
    pub source_elements: Vec<ElementItem>,
    /// Data `SimpleItem` pairs (e.g. `IsMotion = "true"`).
    pub data: HashMap<String, String>,
    /// Data `ElementItem` values in message order.
    pub data_elements: Vec<ElementItem>,
}

/// A structured ONVIF event item retained without interpreting its vendor schema.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementItem {
    /// Value of the `ElementItem` `Name` attribute.
    pub name: String,
    /// Namespace-normalized canonical XML for the item's structured content.
    pub content: String,
    /// Whether the content exceeded [`MAX_ELEMENT_ITEM_BYTES`] and was replaced by a marker.
    pub truncated: bool,
}

/// A push notification and the TCP peer observed by the receiving socket.
///
/// Returned by [`crate::notification_listener_with_peer`]. This is local transport
/// metadata, not an ONVIF XML extension or authenticated device identity. A proxy
/// or NAT may hide the original camera; the port is the connection's source port,
/// not the camera's ONVIF service port. Proxy headers are never trusted here.
///
/// This wrapper deliberately does not derive serde: exporting network addresses
/// is an explicit caller decision. [`NotificationMessage`]'s JSON is unchanged.
#[derive(Debug, Clone)]
pub struct ReceivedNotification {
    /// Parsed ONVIF notification, unchanged from the legacy listener's payload.
    pub message: NotificationMessage,
    /// Direct TCP peer, including the source port of this connection.
    pub peer: std::net::SocketAddr,
}

impl NotificationMessage {
    pub(crate) fn vec_from_xml(resp: &XmlNode) -> Vec<Self> {
        resp.children_named("NotificationMessage")
            .map(Self::from_node)
            .collect()
    }

    fn from_node(node: &XmlNode) -> Self {
        let topic = node
            .child("Topic")
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default();

        // The inner Message element may be wrapped: .../Message/Message[@UtcTime]
        let msg = node
            .path(&["Message", "Message"])
            .or_else(|| node.child("Message"));

        let utc_time = msg
            .and_then(|n| n.attr("UtcTime").map(str::to_string))
            .unwrap_or_default();

        let property_operation = msg
            .and_then(|n| n.attr("PropertyOperation").map(str::to_string))
            .unwrap_or_default();

        let source_node = msg.and_then(|node| node.child("Source"));
        let source = source_node.map(parse_simple_items).unwrap_or_default();
        let source_elements = source_node.map(parse_element_items).unwrap_or_default();

        let data_node = msg.and_then(|node| node.child("Data"));
        let data = data_node.map(parse_simple_items).unwrap_or_default();
        let data_elements = data_node.map(parse_element_items).unwrap_or_default();

        Self {
            topic,
            utc_time,
            property_operation,
            source,
            source_elements,
            data,
            data_elements,
        }
    }
}

fn parse_simple_items(node: &XmlNode) -> HashMap<String, String> {
    node.children_named("SimpleItem")
        .filter_map(|item| {
            let name = item.attr("Name")?.to_string();
            let value = item.attr("Value").unwrap_or("").to_string();
            Some((name, value))
        })
        .collect()
}

fn parse_element_items(node: &XmlNode) -> Vec<ElementItem> {
    node.children_named("ElementItem")
        .map(|item| {
            let name = item.attr("Name").unwrap_or("").to_string();
            let mut content = String::new();
            let complete = append_canonical_content(&mut content, item);
            if complete {
                ElementItem {
                    name,
                    content,
                    truncated: false,
                }
            } else {
                ElementItem {
                    name,
                    content: "<Truncated/>".to_string(),
                    truncated: true,
                }
            }
        })
        .collect()
}

fn append_canonical_node(output: &mut String, node: &XmlNode) -> bool {
    if !append_bounded(output, "<") || !append_bounded(output, &node.local_name) {
        return false;
    }

    let mut attributes: Vec<(&String, &String)> = node.attrs.iter().collect();
    attributes.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (name, value) in attributes {
        if !append_bounded(output, " ")
            || !append_bounded(output, name)
            || !append_bounded(output, "=\"")
            || !append_escaped_attribute(output, value)
            || !append_bounded(output, "\"")
        {
            return false;
        }
    }

    if node.content.is_empty() && node.children.is_empty() && node.text().is_empty() {
        return append_bounded(output, "/>");
    }
    if !append_bounded(output, ">") || !append_canonical_content(output, node) {
        return false;
    }
    append_bounded(output, "</")
        && append_bounded(output, &node.local_name)
        && append_bounded(output, ">")
}

fn append_canonical_content(output: &mut String, node: &XmlNode) -> bool {
    if node.content.is_empty() {
        return append_escaped_text(output, node.text())
            && node
                .children
                .iter()
                .all(|child| append_canonical_node(output, child));
    }
    node.content.iter().all(|part| match part {
        XmlContent::Text(text) => append_escaped_text(output, text),
        XmlContent::Child(index) => node
            .children
            .get(*index)
            .is_some_and(|child| append_canonical_node(output, child)),
    })
}

fn append_escaped_text(output: &mut String, value: &str) -> bool {
    append_escaped(output, value, false)
}

fn append_escaped_attribute(output: &mut String, value: &str) -> bool {
    append_escaped(output, value, true)
}

fn append_escaped(output: &mut String, value: &str, attribute: bool) -> bool {
    for character in value.chars() {
        let replacement = match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute => "&quot;",
            '\'' if attribute => "&apos;",
            _ => {
                let mut buffer = [0u8; 4];
                if !append_bounded(output, character.encode_utf8(&mut buffer)) {
                    return false;
                }
                continue;
            }
        };
        if !append_bounded(output, replacement) {
            return false;
        }
    }
    true
}

fn append_bounded(output: &mut String, value: &str) -> bool {
    let Some(next_len) = output.len().checked_add(value.len()) else {
        return false;
    };
    if next_len > MAX_ELEMENT_ITEM_BYTES {
        return false;
    }
    output.push_str(value);
    true
}

// ── EventProperties ───────────────────────────────────────────────────────────

/// Available event topics advertised by the device.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct EventProperties {
    /// Flattened list of all topic paths (e.g. `VideoSource/MotionAlarm`).
    pub topics: Vec<String>,
}

impl EventProperties {
    pub(crate) fn from_xml(resp: &XmlNode) -> Result<Self, OnvifError> {
        let topic_set = resp
            .child("TopicSet")
            .ok_or_else(|| SoapError::missing("TopicSet"))?;
        let mut topics = Vec::new();
        flatten_topics(topic_set, String::new(), &mut topics);
        Ok(Self { topics })
    }
}

/// Recursively flatten the TopicSet tree into slash-separated paths.
///
/// Leaf nodes are nodes whose children are all `MessageDescription` elements
/// (or that have no children at all). Intermediate nodes become path segments.
fn flatten_topics(node: &XmlNode, prefix: String, out: &mut Vec<String>) {
    for child in &node.children {
        if child.local_name == "MessageDescription" {
            continue;
        }
        let path = if prefix.is_empty() {
            child.local_name.clone()
        } else {
            format!("{prefix}/{}", child.local_name)
        };
        let has_non_desc = child
            .children
            .iter()
            .any(|n| n.local_name != "MessageDescription");
        if has_non_desc {
            flatten_topics(child, path, out);
        } else {
            out.push(path);
        }
    }
}
