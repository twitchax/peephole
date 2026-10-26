//! End to end: the real binary as Claude Code runs it, with an MCP client on its stdio and a real
//! VNC server (rustvncserver) behind it, inside a scratch directory. Tests here are named `e2e_*`,
//! which nextest serializes and retries (see `.config/nextest.toml`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::{collections::BTreeSet, net::SocketAddr, process::Stdio, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use peephole::{
    framebuffer::Rect,
    keys::keysym,
    mcp::APPLE_WHEEL_CLICKS,
    rfb::proto::buttons::{LEFT, WHEEL_DOWN, WHEEL_LEFT},
    tests::pattern_rgb,
};
use pretty_assertions::assert_eq;
use rmcp::{
    RoleClient, ServiceExt as _,
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use support::{
    Input, TestServer, WAIT,
    stalling::{HIGHLIGHT_RGB, MENU_RGB, StallingServer},
    video::VideoServer,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    process::{ChildStderr, Command},
    time::{sleep, timeout},
};

const WIDTH: u16 = 320;
const HEIGHT: u16 = 200;
/// A distinctive password, so a leak anywhere in the output is unambiguous (VNC uses 8 bytes).
const PASSWORD: &str = "s3cr3tPw";

struct Peephole {
    client: RunningService<RoleClient, ()>,
    stderr: ChildStderr,
    _dir: TempDir,
}

/// Starts the binary against `addr` with `args` and `env` on top of a clean environment.
async fn peephole(addr: SocketAddr, args: &[&str], env: &[(&str, &str)]) -> Peephole {
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_peephole"));
    cmd.current_dir(dir.path()).args(["--host", "127.0.0.1", "--port", &addr.port().to_string()]).args(args);
    for name in [
        "PEEPHOLE_HOST",
        "PEEPHOLE_PORT",
        "PEEPHOLE_USER",
        "PEEPHOLE_PASSWORD",
        "PEEPHOLE_PASSWORD_FILE",
        "PEEPHOLE_VIEW_ONLY",
        "PEEPHOLE_MAX_EDGE",
        "PEEPHOLE_IDLE_PAUSE",
        "PEEPHOLE_IDLE_DISCONNECT",
        "RUST_LOG",
    ] {
        cmd.env_remove(name);
    }
    cmd.envs(env.iter().copied());
    let (transport, stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::piped()).spawn().unwrap();
    let client = ().serve(transport).await.unwrap();
    Peephole {
        client,
        stderr: stderr.unwrap(),
        _dir: dir,
    }
}

impl Peephole {
    async fn call(&self, tool: &'static str, args: Value) -> CallToolResult {
        let Value::Object(args) = args else { panic!("arguments must be an object") };
        timeout(WAIT * 2, self.client.call_tool(CallToolRequestParams::new(tool).with_arguments(args)))
            .await
            .unwrap_or_else(|_| panic!("{tool} timed out"))
            .unwrap()
    }

    async fn ok(&self, tool: &'static str, args: Value) -> CallToolResult {
        let result = self.call(tool, args).await;
        assert_eq!(result.is_error, Some(false), "{tool} failed: {}", text(&result));
        result
    }

    async fn tools(&self) -> BTreeSet<String> {
        self.client.list_all_tools().await.unwrap().into_iter().map(|tool| tool.name.into_owned()).collect()
    }

    /// Shuts the client down and returns everything the binary wrote to stderr.
    async fn finish(self) -> String {
        self.client.cancel().await.unwrap();
        let mut stderr = String::new();
        timeout(WAIT, BufReader::new(self.stderr).read_to_string(&mut stderr)).await.expect("the binary did not exit").unwrap();
        stderr
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The decoded PNG in a screenshot result: (width, height, RGB8 pixels).
fn image(result: &CallToolResult) -> (u32, u32, Vec<u8>) {
    let data = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Image(image) if image.mime_type == "image/png" => Some(BASE64.decode(&image.data).unwrap()),
            _ => None,
        })
        .expect("a PNG image block");
    let mut reader = png::Decoder::new(std::io::Cursor::new(data)).read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    pixels.truncate(info.buffer_size());
    (info.width, info.height, pixels)
}

fn key_taps(keys: &[u32]) -> Vec<Input> {
    keys.iter().flat_map(|&k| [Input::Key(k, true), Input::Key(k, false)]).collect()
}

#[tokio::test]
async fn e2e_screenshot_shows_the_remote_screen() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;

    let shot = app.ok("screenshot", json!({})).await;

    let (width, height, pixels) = image(&shot);
    assert_eq!((width, height), (u32::from(WIDTH), u32::from(HEIGHT)));
    assert!(pixels == pattern_rgb(WIDTH, HEIGHT), "the screenshot must match the server's framebuffer exactly");
    assert!(text(&shot).contains("of the whole screen, in screen space"), "{}", text(&shot));

    let info = text(&app.ok("screen_info", json!({})).await);
    assert!(info.contains("state: connected to \"peephole-test\""), "{info}");
    assert!(info.contains("framebuffer: 320x200"), "{info}");
    app.finish().await;
}

#[tokio::test]
async fn e2e_input_tools_put_events_on_the_wire() {
    let mut server = TestServer::start(WIDTH, HEIGHT, None).await;
    let app = peephole(server.addr, &[], &[]).await;

    app.ok("click", json!({"x": 10, "y": 20})).await;
    assert_eq!(server.input_events(3).await, [Input::Pointer(10, 20, 0), Input::Pointer(10, 20, LEFT), Input::Pointer(10, 20, 0)]);

    app.ok("key", json!({"combo": "cmd+shift+g"})).await;
    assert_eq!(
        server.input_events(6).await,
        [
            Input::Key(keysym::SUPER_L, true),
            Input::Key(keysym::SHIFT_L, true),
            Input::Key(u32::from('g'), true),
            Input::Key(u32::from('g'), false),
            Input::Key(keysym::SHIFT_L, false),
            Input::Key(keysym::SUPER_L, false),
        ]
    );

    app.ok("type_text", json!({"text": "Hé\n"})).await;
    assert_eq!(server.input_events(6).await, key_taps(&[u32::from('H'), 0xe9, keysym::RETURN]));

    app.ok("scroll", json!({"x": 5, "y": 6, "dy": 2})).await;
    assert_eq!(
        server.input_events(5).await,
        [
            Input::Pointer(5, 6, 0),
            Input::Pointer(5, 6, WHEEL_DOWN),
            Input::Pointer(5, 6, 0),
            Input::Pointer(5, 6, WHEEL_DOWN),
            Input::Pointer(5, 6, 0),
        ]
    );

    app.ok("move", json!({"x": 300, "y": 199})).await;
    assert_eq!(server.input_events(1).await, [Input::Pointer(300, 199, 0)]);

    app.ok("drag", json!({"from": {"x": 0, "y": 0}, "to": {"x": 120, "y": 60}})).await;
    let drag = server.input_events(15).await;
    assert_eq!(drag[..2], [Input::Pointer(0, 0, 0), Input::Pointer(0, 0, LEFT)]);
    assert_eq!(drag[13..], [Input::Pointer(120, 60, LEFT), Input::Pointer(120, 60, 0)]);

    let bad = app.call("click", json!({"x": 320, "y": 0})).await;
    assert_eq!(bad.is_error, Some(true));
    assert!(text(&bad).contains("outside the 320x200 screen"), "{}", text(&bad));
    app.finish().await;
}

#[tokio::test]
async fn e2e_input_coordinates_are_in_screen_space() {
    let mut server = TestServer::start(WIDTH, HEIGHT, None).await;
    // Screen space is 160x100, so each screen pixel covers 2x2 framebuffer pixels.
    let app = peephole(server.addr, &["--max-edge", "160"], &[]).await;

    let shot = app.ok("screenshot", json!({})).await;
    assert_eq!(image(&shot).0, 160);

    app.ok("move", json!({"x": 10, "y": 20})).await;
    assert_eq!(server.input_events(1).await, [Input::Pointer(21, 41, 0)]);

    let zoom = app.ok("screenshot", json!({"region": {"x": 80, "y": 50, "width": 80, "height": 50}})).await;
    let (width, height, _) = image(&zoom);
    assert_eq!((width, height), (160, 100), "a zoom shows the region at framebuffer resolution");
    app.finish().await;
}

/// Runs `call` while the server shows `changes` new frames, one every 100 ms, then goes still.
async fn while_animating(server: &TestServer, call: impl Future<Output = CallToolResult>, changes: u8) -> CallToolResult {
    tokio::pin!(call);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut shown = 0u8;
    loop {
        tokio::select! {
            result = &mut call => return result,
            _ = ticker.tick(), if shown < changes => {
                shown += 1;
                server.show(&vec![shown; usize::from(WIDTH) * usize::from(HEIGHT) * 3]).await;
            }
        }
    }
}

/// The number before " ms" that follows `label` in `text`.
fn ms_after(text: &str, label: &str) -> u64 {
    let rest = &text[text.find(label).unwrap_or_else(|| panic!("{label:?} in {text:?}")) + label.len()..];
    rest.trim_start().split(' ').next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn e2e_wait_for_change_returns_at_the_first_change_without_settling() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;
    app.ok("screenshot", json!({})).await;

    let quiet = app.ok("wait_for_change", json!({"timeout_ms": 300})).await;
    assert!(text(&quiet).starts_with("no change"), "{}", text(&quiet));

    // The screen changes for 20 s; with settle_ms 0 the wait returns at the first change.
    let result = while_animating(&server, app.call("wait_for_change", json!({"timeout_ms": 20_000, "settle_ms": 0})), 200).await;
    assert!(ms_after(&text(&result), "changed after") < 5_000, "{}", text(&result));
    app.finish().await;
}

#[tokio::test]
async fn e2e_wait_for_change_waits_for_the_screen_to_settle() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;
    app.ok("screenshot", json!({})).await;

    // Eight frames over 0.8 s, like an animation, then stillness.
    let result = while_animating(&server, app.call("wait_for_change", json!({"timeout_ms": 20_000})), 8).await;

    let result = text(&result);
    assert!(ms_after(&result, "settled after") >= 400, "the wait must outlast the animation: {result}");
    assert!(ms_after(&result, "changed after") <= ms_after(&result, "settled after"), "{result}");
    app.finish().await;
}

#[tokio::test]
async fn e2e_wait_for_change_reports_a_screen_that_never_settles() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    let app = peephole(server.addr, &[], &[]).await;
    app.ok("screenshot", json!({})).await;

    let result = while_animating(&server, app.call("wait_for_change", json!({"timeout_ms": 1_000})), 40).await;

    assert!(text(&result).contains("still changing"), "{}", text(&result));
    app.finish().await;
}

fn pixel((width, _, pixels): &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 3] {
    let at = usize::try_from((y * width + x) * 3).unwrap();
    pixels[at..at + 3].try_into().unwrap()
}

#[tokio::test]
async fn e2e_a_screenshot_after_input_fetches_fresh_pixels() {
    let server = StallingServer::start(WIDTH, HEIGHT, pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;
    let full = Rect::new(0, 0, WIDTH, HEIGHT);

    app.ok("screenshot", json!({})).await;
    assert_eq!(server.fetches(), [full], "with no input yet, the cache is current");

    // The click opens a menu that incremental updates never bring.
    app.ok("click", json!({"x": 40, "y": 4})).await;
    let zoom = image(&app.ok("screenshot", json!({"region": {"x": 0, "y": 0, "width": 96, "height": 80}})).await);
    assert_eq!(pixel(&zoom, 40, 40), MENU_RGB);
    let shot = image(&app.ok("screenshot", json!({})).await);
    assert_eq!(pixel(&shot, 40, 40), MENU_RGB);
    app.ok("screenshot", json!({})).await;

    assert_eq!(
        server.fetches(),
        [full, Rect::new(0, 0, 96, 80), full],
        "a region fetches just itself; the whole frame is fetched once per input"
    );
    app.finish().await;
}

#[tokio::test]
async fn e2e_wait_for_change_after_input_checks_the_settle_against_fresh_pixels() {
    let server = StallingServer::start(WIDTH, HEIGHT, pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;
    app.ok("screenshot", json!({})).await;

    app.ok("click", json!({"x": 40, "y": 4})).await;
    let waited = text(&app.ok("wait_for_change", json!({"timeout_ms": 20_000})).await);
    assert!(waited.contains("settled after"), "{waited}");

    // The highlight bands go quiet with the menu still held back: one fresh copy finds the menu,
    // and a second confirms it has stopped changing.
    let full = Rect::new(0, 0, WIDTH, HEIGHT);
    assert_eq!(server.fetches(), [full, full, full], "{waited}");
    // That copy is the cache now, so the screenshot fetches nothing.
    let shot = image(&app.ok("screenshot", json!({})).await);
    assert_eq!(pixel(&shot, 40, 40), MENU_RGB);
    assert_eq!(pixel(&shot, 70, 4), HIGHLIGHT_RGB);
    assert_eq!(server.fetches().len(), 3);
    app.finish().await;
}

#[tokio::test]
async fn e2e_scroll_on_a_mac_sends_a_line_of_wheel_clicks_per_notch() {
    let mut server = StallingServer::start(WIDTH, HEIGHT, pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;

    // Apple's server scrolls about a point per wheel click, so each notch is many clicks.
    app.ok("scroll", json!({"x": 5, "y": 6, "dy": 1, "dx": -1})).await;
    let clicks = usize::try_from(APPLE_WHEEL_CLICKS).unwrap();
    let notch = |wheel| [Input::Pointer(5, 6, wheel), Input::Pointer(5, 6, 0)].repeat(clicks);
    let expected = [vec![Input::Pointer(5, 6, 0)], notch(WHEEL_DOWN), notch(WHEEL_LEFT)].concat();
    assert_eq!(server.input_events(expected.len()).await, expected);
    app.finish().await;
}

#[tokio::test]
async fn e2e_the_idle_flags_set_when_the_stream_pauses_and_the_session_disconnects() {
    let server = VideoServer::start(WIDTH, HEIGHT).await;
    let app = peephole(server.addr, &["--idle-pause", "1", "--idle-disconnect", "3"], &[]).await;

    app.ok("screen_info", json!({})).await;
    // Past the pause and short of the disconnect, so the stream has stopped and the connection
    // is still open.
    sleep(Duration::from_secs(2)).await;
    let paused = server.requests().len();
    server.closed().await;
    assert_eq!(server.requests().len(), paused, "the stream ran past --idle-pause");
    app.finish().await;
}

#[tokio::test]
async fn e2e_view_only_hides_every_input_tool() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    let full = peephole(server.addr, &[], &[]).await;
    let all_tools = full.tools().await;
    full.finish().await;
    let app = peephole(server.addr, &["--view-only"], &[]).await;

    assert_eq!(all_tools.len(), 9);
    assert_eq!(app.tools().await, BTreeSet::from(["screen_info".to_owned(), "screenshot".to_owned(), "wait_for_change".to_owned()]));
    let refused = app
        .client
        .call_tool(CallToolRequestParams::new("click").with_arguments(json!({"x": 1, "y": 1}).as_object().unwrap().clone()))
        .await;
    assert!(refused.is_err() || refused.as_ref().is_ok_and(|r| r.is_error == Some(true)), "{refused:?}");
    // The first call connects, even one that only reports.
    let info = text(&app.ok("screen_info", json!({})).await);
    assert!(info.contains("state: connected") && info.contains("input: disabled"), "{info}");
    app.finish().await;
}

#[tokio::test]
async fn e2e_never_echoes_the_password() {
    let server = TestServer::start(WIDTH, HEIGHT, Some(PASSWORD)).await;
    let app = peephole(server.addr, &[], &[("PEEPHOLE_PASSWORD", PASSWORD), ("RUST_LOG", "trace")]).await;

    let mut output = text(&app.ok("screenshot", json!({})).await);
    output += &text(&app.ok("screen_info", json!({})).await);
    let stderr = app.finish().await;

    assert!(stderr.contains("connected"), "trace logging should show the session: {stderr}");
    assert!(!output.contains(PASSWORD), "tool output leaked the password: {output}");
    assert!(!stderr.contains(PASSWORD), "stderr leaked the password");
}

#[tokio::test]
async fn e2e_reads_the_password_from_a_file() {
    let server = TestServer::start(WIDTH, HEIGHT, Some(PASSWORD)).await;
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("password");
    std::fs::write(&file, format!("{PASSWORD}\n")).unwrap();
    let app = peephole(server.addr, &["--password-file", file.to_str().unwrap()], &[]).await;
    app.ok("screenshot", json!({})).await;
    app.finish().await;
}

#[tokio::test]
async fn e2e_stops_after_a_rejected_login() {
    let server = TestServer::start(WIDTH, HEIGHT, Some(PASSWORD)).await;
    let app = peephole(server.addr, &[], &[("PEEPHOLE_PASSWORD", "wrong-pw")]).await;

    let refused = app.call("screenshot", json!({})).await;

    assert_eq!(refused.is_error, Some(true));
    assert!(text(&refused).contains("rejected the login"), "{}", text(&refused));
    assert!(!text(&refused).contains("wrong-pw"));
    assert!(text(&app.ok("screen_info", json!({})).await).contains("state: stopped"));
    app.finish().await;
}

#[test]
fn e2e_fails_cleanly_without_a_host() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_peephole")).env_remove("PEEPHOLE_HOST").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--host"));
}
