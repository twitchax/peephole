//! The session's idle behaviour, through the library: it connects for the first tool, stops asking
//! for updates when no tool calls, catches up when one does, and lets go of the server after a
//! longer idle. The server is a mock whose screen never stops changing, like a screensaver.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::{net::SocketAddr, time::Duration};

use peephole::{
    credentials::Credentials,
    framebuffer::Rect,
    session::{Change, Idle, Session, SessionConfig, State},
};
use pretty_assertions::assert_eq;
use support::{
    WAIT,
    video::{FRAME, Request, VideoServer},
};
use tokio::time::sleep;

const WIDTH: u16 = 64;
const HEIGHT: u16 = 48;
const FULL: Rect = Rect::new(0, 0, WIDTH, HEIGHT);
/// Short, so the tests take seconds; every wait below allows several of these.
const PAUSE: Duration = Duration::from_millis(300);
/// Pauses, but stays connected for the length of a test.
const PAUSING: Idle = Idle {
    pause: PAUSE,
    disconnect: Duration::from_mins(1),
};
/// Painted while nobody is looking, outside `FRAME`, so only a full fetch brings it.
const MARK: Rect = Rect::new(32, 24, 8, 8);
const MARK_RGB: [u8; 3] = [250, 20, 120];

fn session(addr: SocketAddr, view_only: bool, idle: Idle) -> Session {
    Session::spawn(SessionConfig {
        host: "127.0.0.1".to_owned(),
        port: addr.port(),
        credentials: Credentials::default(),
        view_only,
        idle,
    })
}

fn pixel(session: &Session, at: Rect) -> [u8; 3] {
    session.framebuffer().crop(&Rect::new(at.x, at.y, 1, 1)).unwrap().try_into().unwrap()
}

/// Waits out the stream after the last tool, and returns the requests made by then.
async fn paused(server: &VideoServer) -> usize {
    sleep(PAUSE * 4).await;
    server.requests().len()
}

#[tokio::test]
async fn connects_for_the_first_tool_and_stops_asking_when_no_tool_calls() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let session = session(server.addr, false, PAUSING);

    // An eager session would have connected by now.
    sleep(PAUSE).await;
    assert_eq!((server.connections(), session.state()), (0, State::Idle));

    session.ready(WAIT).await.unwrap();
    let asked = paused(&server).await;
    let incremental = server.requests().iter().filter(|r| r.incremental).count();
    assert!(incremental >= 2, "the stream runs while in use: {:?}", server.requests());
    sleep(PAUSE * 3).await;
    assert_eq!(server.requests().len(), asked, "an idle session asks for nothing");

    // The screen moved on unseen, so the next tool waits for the whole frame, and the stream
    // starts again.
    server.paint(MARK, MARK_RGB);
    session.ready(WAIT).await.unwrap();
    assert_eq!(pixel(&session, MARK), MARK_RGB);
    let resumed = server.wait_for_requests(asked + 2).await;
    assert_eq!(resumed[asked], Request { incremental: false, area: FULL });
    assert!(resumed[asked + 1].incremental, "{resumed:?}");
}

#[tokio::test]
async fn a_view_only_session_catches_up_after_a_pause_too() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let session = session(server.addr, true, PAUSING);

    session.ready(WAIT).await.unwrap();
    paused(&server).await;
    server.paint(MARK, MARK_RGB);
    session.ready(WAIT).await.unwrap();
    assert_eq!(pixel(&session, MARK), MARK_RGB);
}

#[tokio::test]
async fn a_long_wait_keeps_the_stream_going() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let session = session(server.addr, false, PAUSING);
    session.ready(WAIT).await.unwrap();

    // Had the stream paused a `PAUSE` in, the wait would see it settle well before the timeout.
    let change = session.wait_for_change(FRAME, PAUSE * 5, PAUSE * 3).await.unwrap();
    assert!(matches!(change, Change::Unsettled { .. }), "{change:?}");
}

#[tokio::test]
async fn idle_periods_too_long_for_the_clock_never_end() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let session = session(
        server.addr,
        false,
        Idle {
            pause: Duration::MAX,
            disconnect: Duration::MAX,
        },
    );

    session.ready(WAIT).await.unwrap();
    let asked = server.requests().len();
    server.wait_for_requests(asked + 3).await;
    assert_eq!(server.connections(), 1);
    assert!(matches!(session.state(), State::Connected(_)), "{}", session.state());
}

#[tokio::test]
async fn lets_go_of_the_server_after_a_longer_idle_and_reconnects_for_the_next_tool() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let session = session(server.addr, false, Idle { pause: PAUSE, disconnect: PAUSE * 2 });

    session.ready(WAIT).await.unwrap();
    server.closed().await;
    assert_eq!(session.state(), State::Idle);

    server.paint(MARK, MARK_RGB);
    session.ready(WAIT).await.unwrap();
    assert_eq!(server.connections(), 2);
    assert_eq!(pixel(&session, MARK), MARK_RGB);
}
