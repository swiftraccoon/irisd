//! Sessions dropped against a camera that never answers `TEARDOWN` hold a
//! bounded number of background tasks.
//!
//! A test binary of its own: the bound is process-wide, and filling it would
//! skip the `TEARDOWN`s other tests wait for.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use iris_replay::rtsp::{self, Parsed};
use iris_replay::{sdp, synth};
use retina::client::{
    Credentials, MAX_BACKGROUND_TEARDOWNS, Session, SessionOptions, SetupOptions,
    TcpTransportOptions, TeardownPolicy, Transport, background_teardowns,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// `TEARDOWN`s a camera received, on a session's own connection and on fresh
/// ones.
#[derive(Default)]
struct Teardowns {
    own_connection: AtomicUsize,
    fresh: AtomicUsize,
}

/// Answers `DESCRIBE` and `SETUP`; reads `TEARDOWN` and never answers it.
async fn serve(mut socket: TcpStream, teardowns: Arc<Teardowns>) {
    let mut input = Vec::new();
    let mut set_up = false;
    loop {
        let request = match rtsp::parse(&input) {
            Parsed::Request(request, consumed) => {
                input.drain(..consumed);
                request
            }
            Parsed::Interleaved(consumed) => {
                input.drain(..consumed);
                continue;
            }
            Parsed::Incomplete => match socket.read_buf(&mut input).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            },
            Parsed::Invalid => return,
        };
        let reply = match request.method.as_str() {
            "DESCRIBE" => {
                let sets = [
                    synth::TEST_VPS.to_vec(),
                    synth::TEST_SPS.to_vec(),
                    synth::TEST_PPS.to_vec(),
                ];
                let body = sdp::describe(&sets, 30.0);
                rtsp::response(
                    request.cseq,
                    200,
                    "OK",
                    &[
                        ("Content-Type", "application/sdp".to_owned()),
                        ("Content-Base", format!("{}/", request.url)),
                    ],
                    body.as_bytes(),
                )
            }
            "SETUP" => {
                set_up = true;
                rtsp::response(
                    request.cseq,
                    200,
                    "OK",
                    &[
                        ("Session", "unanswered;timeout=60".to_owned()),
                        (
                            "Transport",
                            "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678".to_owned(),
                        ),
                    ],
                    b"",
                )
            }
            "TEARDOWN" => {
                let counter = if set_up {
                    &teardowns.own_connection
                } else {
                    &teardowns.fresh
                };
                counter.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            _ => rtsp::response(request.cseq, 200, "OK", &[], b""),
        };
        if socket.write_all(&reply).await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn dropped_sessions_hold_a_bounded_number_of_teardown_tasks() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind camera");
        let address = listener.local_addr().expect("camera address");
        let teardowns = Arc::new(Teardowns::default());
        let camera = {
            let teardowns = Arc::clone(&teardowns);
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve(socket, Arc::clone(&teardowns)));
                }
            })
        };
        let url: iris_ingest::Url = format!("rtsp://{address}/cam").parse().expect("RTSP URL");

        let dropped = MAX_BACKGROUND_TEARDOWNS + 36;
        for _ in 0..dropped {
            let options = SessionOptions::default()
                .creds(Some(Credentials {
                    username: "user".to_owned(),
                    password: "secret".to_owned(),
                }))
                .teardown(TeardownPolicy::Always);
            let mut session = tokio::time::timeout(
                Duration::from_secs(5),
                Session::describe(url.clone(), options),
            )
            .await
            .expect("DESCRIBE answered")
            .expect("DESCRIBE succeeded");
            tokio::time::timeout(
                Duration::from_secs(5),
                session.setup(
                    0,
                    SetupOptions::default()
                        .transport(Transport::Tcp(TcpTransportOptions::default())),
                ),
            )
            .await
            .expect("SETUP answered")
            .expect("SETUP succeeded");
            drop(session);
        }

        // Every running task waits on its unanswered TEARDOWN; the sessions
        // dropped beyond the bound were not torn down at all.
        assert_eq!(background_teardowns(), MAX_BACKGROUND_TEARDOWNS);
        let all_sent = tokio::time::timeout(Duration::from_secs(5), async {
            while teardowns.own_connection.load(Ordering::SeqCst) < MAX_BACKGROUND_TEARDOWNS {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        assert!(all_sent, "not every running task sent its TEARDOWN");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            teardowns.own_connection.load(Ordering::SeqCst),
            MAX_BACKGROUND_TEARDOWNS,
            "{dropped} sessions dropped"
        );
        assert_eq!(background_teardowns(), MAX_BACKGROUND_TEARDOWNS);
        camera.abort();
    })
    .await
    .expect("teardown-bound regression stayed bounded");
}
