//! The bounds `iris-client` documents as the daemon's, each against the
//! constant the daemon applies. A viewer refuses a request or an answer
//! beyond its bound, so a bound that drifts from the daemon's refuses what
//! the daemon accepts or sends.

use std::cmp::Ordering;

use iris_client::{api, live, vod};
use iris_core::{config, event};
use iris_events::{discovery, media};

use crate::live::{MAX_LIVE_MESSAGE_BYTES, MSG_FRAGMENT, MSG_INIT};
use crate::query::MAX_RANGE_US;
use crate::{events, hls, onvif};

#[test]
fn the_discovery_wait_is_the_range_the_daemon_takes() {
    assert_eq!(api::DISCOVERY_WAIT_MS, onvif::DISCOVERY_WAIT_MS);
}

#[test]
fn a_display_string_is_bounded_as_an_event_kind() {
    assert_eq!(api::MAX_DISPLAY_STRING_BYTES, event::MAX_EVENT_KIND_LEN);
}

#[test]
fn the_discovered_devices_are_bounded_as_the_daemon_bounds_them() {
    assert_eq!(api::MAX_DISCOVERED_DEVICES, discovery::MAX_DISCOVERED);
}

#[test]
fn the_profile_streams_are_bounded_as_the_daemon_bounds_them() {
    assert_eq!(api::MAX_PROFILE_STREAMS, media::MAX_STREAM_PROFILES);
}

#[test]
fn a_label_is_bounded_as_the_text_the_daemon_sends() {
    assert_eq!(api::MAX_LABEL_CHARS, discovery::MAX_TEXT_CHARS);
}

#[test]
fn an_endpoint_is_bounded_as_the_daemon_bounds_it() {
    assert_eq!(api::MAX_ENDPOINT_CHARS, discovery::MAX_ENDPOINT_CHARS);
}

#[test]
fn a_host_is_bounded_as_a_configured_host() {
    assert_eq!(api::MAX_HOST_BYTES, config::MAX_HOST_BYTES);
}

#[test]
fn a_stream_url_is_bounded_as_the_daemon_bounds_an_advertised_one() {
    assert_eq!(
        api::MAX_STREAM_URL_BYTES,
        iris_events::MAX_ADVERTISED_URL_LEN
    );
}

#[test]
fn the_segments_of_a_list_are_bounded_as_the_rows_the_daemon_answers() {
    assert_eq!(api::MAX_SEGMENTS, hls::MAX_SEGMENT_ROWS);
}

#[test]
fn the_events_of_a_page_are_bounded_as_the_largest_page() {
    assert_eq!(api::MAX_EVENTS, events::MAX_LIMIT);
}

#[test]
fn the_longest_range_is_the_longest_the_daemon_takes() {
    assert_eq!(api::MAX_RANGE_US, i128::from(MAX_RANGE_US));
}

#[test]
fn a_keyframe_bound_is_the_sum_its_doc_states() {
    let daemon = hls::MAX_INIT_BYTES + hls::MAX_FRAGMENT_BYTES + (1 << 20);
    assert_eq!(u64::try_from(api::MAX_KEYFRAME_BYTES), Ok(daemon));
}

#[test]
fn the_fragments_of_a_playlist_are_bounded_as_the_records_the_daemon_selects() {
    assert_eq!(vod::MAX_PLAYBACK_FRAGMENTS, hls::MAX_SELECTED_RECORDS);
}

#[test]
fn the_daemon_renders_a_shorter_playlist_than_a_viewer_takes() {
    let daemon = hls::MAX_PLAYLIST_BYTES;
    assert_eq!(daemon.cmp(&vod::MAX_VOD_PLAYLIST_BYTES), Ordering::Less);
}

#[test]
fn an_init_message_has_the_type_byte_the_daemon_writes() {
    assert_eq!(live::MSG_INIT, MSG_INIT);
}

#[test]
fn a_fragment_message_has_the_type_byte_the_daemon_writes() {
    assert_eq!(live::MSG_FRAGMENT, MSG_FRAGMENT);
}

#[test]
fn a_live_message_has_the_ceiling_the_daemon_keeps() {
    assert_eq!(live::LIVE_MESSAGE_BYTES, MAX_LIVE_MESSAGE_BYTES);
}
