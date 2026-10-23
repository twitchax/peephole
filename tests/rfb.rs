//! The RFB client against a real server (rustvncserver): handshake, decoding and input on the
//! wire, below the session and MCP layers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use peephole::{
    credentials::{Credentials, Secret},
    framebuffer::Framebuffer,
    rfb::{
        AuthError, ClientMessage, MessageReader, PixelFormat, ServerMessage, Version, handshake,
        proto::{buttons, encoding, security},
    },
    tests::pattern_rgb,
};
use pretty_assertions::assert_eq;
use support::{Input, TestServer, WAIT};
use tokio::{
    io::{AsyncWriteExt as _, BufReader},
    net::TcpStream,
    time::{Instant, timeout},
};

const WIDTH: u16 = 150;
const HEIGHT: u16 = 90;

fn password(p: &str) -> Credentials {
    Credentials {
        username: None,
        password: Some(Secret::new(p.to_owned())),
    }
}

async fn send(stream: &mut BufReader<TcpStream>, messages: &[ClientMessage]) {
    let mut out = Vec::new();
    for message in messages {
        message.encode(&mut out);
    }
    stream.get_mut().write_all(&out).await.unwrap();
}

/// Reads and applies updates until `fb` shows `expected`.
async fn read_until(stream: &mut BufReader<TcpStream>, reader: &mut MessageReader, fb: &mut Framebuffer, expected: &[u8]) {
    let deadline = Instant::now() + WAIT;
    while fb.pixels() != expected {
        let message = timeout(deadline.saturating_duration_since(Instant::now()), reader.read(stream, (fb.width(), fb.height())))
            .await
            .expect("timed out waiting for the framebuffer to match")
            .unwrap();
        if message == ServerMessage::FramebufferUpdate {
            reader.apply(fb).unwrap();
        }
    }
}

/// Connects, negotiates `encodings`, and returns a client holding the server's first full frame.
async fn connect(server: &TestServer, credentials: &Credentials, encodings: &'static [i32]) -> (BufReader<TcpStream>, MessageReader, Framebuffer) {
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    let done = handshake(&mut tcp, credentials).await.unwrap();
    assert_eq!(done.version, Version::V3_8);
    assert_eq!((done.init.width, done.init.height), (WIDTH, HEIGHT));

    let mut stream = BufReader::new(tcp);
    let mut fb = Framebuffer::new(WIDTH, HEIGHT);
    send(
        &mut stream,
        &[
            ClientMessage::SetPixelFormat(PixelFormat::RGBX),
            ClientMessage::SetEncodings(encodings),
            ClientMessage::UpdateRequest { incremental: false, area: fb.bounds() },
        ],
    )
    .await;
    let mut reader = MessageReader::new();
    read_until(&mut stream, &mut reader, &mut fb, &server.pixels().await).await;
    (stream, reader, fb)
}

#[tokio::test]
async fn decodes_a_raw_frame() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let (_, _, fb) = connect(&server, &Credentials::default(), &[encoding::RAW]).await;
    assert_eq!(fb.pixels(), pattern_rgb(WIDTH, HEIGHT));
}

#[tokio::test]
async fn decodes_a_zrle_frame_and_its_incremental_updates() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let (mut stream, mut reader, mut fb) = connect(&server, &Credentials::default(), &[encoding::ZRLE]).await;
    assert_eq!(fb.pixels(), pattern_rgb(WIDTH, HEIGHT));

    // A second update on the same zlib stream.
    let mut changed = pattern_rgb(WIDTH, HEIGHT);
    changed[..300].fill(0xaa);
    server.show(&changed).await;
    send(&mut stream, &[ClientMessage::UpdateRequest { incremental: true, area: fb.bounds() }]).await;
    read_until(&mut stream, &mut reader, &mut fb, &changed).await;
}

#[tokio::test]
async fn authenticates_with_a_vnc_password() {
    let server = TestServer::start(WIDTH, HEIGHT, Some("s3cret")).await;
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    let done = handshake(&mut tcp, &password("s3cret")).await.unwrap();
    assert_eq!(done.security, security::VNC);
}

#[tokio::test]
async fn reports_a_wrong_vnc_password_as_an_auth_error() {
    let server = TestServer::start(WIDTH, HEIGHT, Some("s3cret")).await;
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    let error = handshake(&mut tcp, &password("wr0ng")).await.unwrap_err();
    assert!(error.is::<AuthError>(), "{error:#}");
    assert!(!format!("{error:#}").contains("wr0ng"));
}

#[tokio::test]
async fn refuses_to_guess_when_the_server_wants_a_password() {
    let server = TestServer::start(WIDTH, HEIGHT, Some("s3cret")).await;
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    let error = handshake(&mut tcp, &Credentials::default()).await.unwrap_err();
    assert!(error.is::<AuthError>(), "{error:#}");
    assert!(format!("{error:#}").contains("PEEPHOLE_PASSWORD"), "{error:#}");
}

#[tokio::test]
async fn puts_pointer_and_key_events_on_the_wire() {
    let mut server = TestServer::start(WIDTH, HEIGHT, None).await;
    let (mut stream, _, _) = connect(&server, &Credentials::default(), &[encoding::RAW]).await;
    send(
        &mut stream,
        &[
            ClientMessage::Pointer { x: 12, y: 34, buttons: 0 },
            ClientMessage::Pointer { x: 12, y: 34, buttons: buttons::LEFT },
            ClientMessage::Key { keysym: 0xffeb, down: true },
            ClientMessage::Key { keysym: u32::from('g'), down: true },
            ClientMessage::Key { keysym: u32::from('g'), down: false },
            ClientMessage::Key { keysym: 0xffeb, down: false },
        ],
    )
    .await;
    assert_eq!(
        server.input_events(6).await,
        [
            Input::Pointer(12, 34, 0),
            Input::Pointer(12, 34, buttons::LEFT),
            Input::Key(0xffeb, true),
            Input::Key(0x67, true),
            Input::Key(0x67, false),
            Input::Key(0xffeb, false),
        ]
    );
}
