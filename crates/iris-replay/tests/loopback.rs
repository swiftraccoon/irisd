//! A retina client receives exactly the frames a synthetic recording holds.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use iris_h265::strip_parameter_sets;
use iris_replay::media::Media;
use iris_replay::rtsp::MountName;
use iris_replay::server::{Mounts, Options, Server};
use iris_replay::synth::{self, Plan};
use retina::client::{
    PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions, Transport,
};
use retina::codec::{CodecItem, FrameFormat};
use url::Url;

#[tokio::test]
async fn frames_round_trip_through_a_retina_client() {
    let plan = Plan {
        gops: 2,
        frames_per_gop: 4,
        idr_len: 20_000,
        trail_len: 3_000,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), Arc::clone(&media))]);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, Options::default())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());

    let url = Url::parse(&format!("rtsp://{addr}/cam")).unwrap();
    let mut session = Session::describe(url, SessionOptions::default())
        .await
        .unwrap();
    let index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video")
        .unwrap();
    session
        .setup(
            index,
            SetupOptions::default()
                .transport(Transport::Tcp(TcpTransportOptions::default()))
                .frame_format(FrameFormat::MP4),
        )
        .await
        .unwrap();
    let playing = session.play(PlayOptions::default()).await.unwrap();
    let mut demuxed = playing.demuxed().unwrap();

    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while received.len() < 8 {
        let item = tokio::time::timeout_at(deadline, demuxed.next())
            .await
            .expect("frames within the deadline")
            .expect("stream open")
            .expect("no error");
        if let CodecItem::VideoFrame(frame) = item {
            assert_eq!(frame.loss(), 0);
            received.push((
                frame.is_random_access_point(),
                frame.timestamp().timestamp(),
                frame.into_data(),
            ));
        }
    }
    let first_ts = received[0].1;
    for (i, (is_sync, timestamp, data)) in received.iter().enumerate() {
        let frame = &media.frames()[i % media.frames().len()];
        assert_eq!(*is_sync, frame.is_sync, "frame {i}");
        assert_eq!(
            timestamp - first_ts,
            i64::try_from(i).unwrap() * 3_000,
            "frame {i} timestamp"
        );
        let payload = strip_parameter_sets(data)
            .unwrap()
            .unwrap_or_else(|| data.clone());
        assert_eq!(Some(&payload[..]), media.sample(frame), "frame {i} bytes");
    }
}
