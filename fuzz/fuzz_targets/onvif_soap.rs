//! Fuzzes the bounded SOAP/XML DOM used by ONVIF responses.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxvif::soap::xml::{MAX_XML_ATTRIBUTES, MAX_XML_DEPTH, MAX_XML_NODES, MAX_XML_TEXT_BYTES};
use oxvif::soap::{XmlNode, parse_soap_body};

fn assert_resource_invariants(root: &XmlNode) {
    let mut stack = vec![(root, 1usize)];
    let mut nodes = 0usize;
    let mut attributes = 0usize;
    let mut text_bytes = 0usize;
    while let Some((node, depth)) = stack.pop() {
        assert!(depth <= MAX_XML_DEPTH, "depth {depth}");
        nodes += 1;
        attributes += node.attrs.len();
        text_bytes += node.text.as_deref().map_or(0, str::len);
        assert!(nodes <= MAX_XML_NODES, "{nodes} nodes");
        assert!(attributes <= MAX_XML_ATTRIBUTES, "{attributes} attributes");
        assert!(text_bytes <= MAX_XML_TEXT_BYTES, "{text_bytes} text bytes");
        stack.extend(node.children.iter().map(|child| (child, depth + 1)));
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(xml) = std::str::from_utf8(data) {
        if let Ok(root) = XmlNode::parse(xml) {
            assert_resource_invariants(&root);
        }
        if let Ok(body) = parse_soap_body(xml) {
            assert_resource_invariants(&body);
        }
    }
});
