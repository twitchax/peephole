//! End to end: the real binary as Claude Code runs it, with an MCP client on its stdio and a real
//! VNC server (rustvncserver) behind it, inside a scratch directory. Tests here are named `e2e_*`,
//! which nextest serializes and retries (see `.config/nextest.toml`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::{collections::BTreeSet, net::SocketAddr, process::Stdio, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use peephole::{
    keys::keysym,
    rfb::proto::buttons::{LEFT, WHEEL_DOWN},
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
use support::{Input, TestServer, WAIT};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    process::{ChildStderr, Command},
    time::timeout,
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

#[tokio::test]
async fn e2e_wait_for_change_sees_the_screen_change() {
    let server = TestServer::start(WIDTH, HEIGHT, None).await;
    server.show(&pattern_rgb(WIDTH, HEIGHT)).await;
    let app = peephole(server.addr, &[], &[]).await;
    app.ok("screenshot", json!({})).await;

    let quiet = app.ok("wait_for_change", json!({"timeout_ms": 300})).await;
    assert!(text(&quiet).starts_with("no change"), "{}", text(&quiet));

    // Keep changing the screen until the wait returns, so the test cannot race the wait's start.
    let result = {
        let waiting = app.call("wait_for_change", json!({"timeout_ms": 20_000}));
        tokio::pin!(waiting);
        let mut frame = 0u8;
        loop {
            frame = frame.wrapping_add(1);
            server.show(&vec![frame; usize::from(WIDTH) * usize::from(HEIGHT) * 3]).await;
            if let Ok(result) = timeout(Duration::from_millis(200), &mut waiting).await {
                break result;
            }
        }
    };
    assert!(text(&result).starts_with("changed after"), "{}", text(&result));
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
    assert!(text(&app.ok("screen_info", json!({})).await).contains("input: disabled"));
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
