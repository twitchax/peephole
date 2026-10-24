//! The MCP surface: the tools an agent calls, over stdio.
//!
//! Every coordinate is in screen space (see [`crate::screenshot`]). Input tools exist only when the
//! session has an input channel; with `--view-only` they are removed from the router, so they are
//! neither listed nor callable.

use std::{fmt::Write as _, sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ServerHandler, ServiceExt as _,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::time::sleep;

use crate::base::{Res, Void};
use crate::framebuffer::Framebuffer;
use crate::keys::{Combo, Stroke, text_strokes};
use crate::rfb::proto::{buttons, security};
use crate::screenshot::{Region, ScreenSpace, Shot, render};
use crate::session::{Change, Input, Session, SessionConfig, State};

/// The tools that send input; `--view-only` removes them.
pub const INPUT_TOOLS: [&str; 6] = ["click", "move", "drag", "scroll", "type_text", "key"];

/// How long a tool waits for a connection before giving up.
const READY_WAIT: Duration = Duration::from_secs(15);
/// How long a button is held for a click, and the pause between the clicks of a double click.
/// Remote desktops read events as they arrive; with no gap at all, some apps miss the press.
const CLICK_HOLD: Duration = Duration::from_millis(20);
const CLICK_GAP: Duration = Duration::from_millis(60);
/// Drags move in steps, so apps see motion rather than a jump.
const DRAG_STEPS: u32 = 12;
const DRAG_STEP: Duration = Duration::from_millis(15);
/// A pause between typed characters, so servers that coalesce rapid events keep them in order.
const KEY_GAP: Duration = Duration::from_millis(5);
const MAX_TEXT: usize = 10_000;
const MAX_SCROLL: u32 = 100;
const DEFAULT_WAIT_MS: u64 = 5_000;
const MAX_WAIT_MS: u64 = 60_000;
/// Longer than the session's 100 ms update pacing, so "still" means several quiet updates, and
/// about as long as a macOS menu or sheet animation.
const DEFAULT_SETTLE_MS: u64 = 300;
const MAX_SETTLE_MS: u64 = 5_000;
const MAX_EDGE_RANGE: std::ops::RangeInclusive<u32> = 16..=8192;

const INSTRUCTIONS: &str = "peephole sees and drives a remote desktop over VNC. \
Coordinates for click, move, drag and scroll are in screen space: the pixel grid of a full `screenshot` taken without `max_edge`, \
so read them straight off that image. A `region` screenshot zooms in without changing screen space; its text explains the mapping. \
After an action, call `wait_for_change` instead of polling screenshots. On a Mac, `cmd` in `key` is the Command key.";

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
pub struct RegionParam {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl From<RegionParam> for Region {
    fn from(r: RegionParam) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

impl Button {
    fn mask(self) -> u8 {
        match self {
            Self::Left => buttons::LEFT,
            Self::Middle => buttons::MIDDLE,
            Self::Right => buttons::RIGHT,
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ScreenshotParams {
    /// Zoom into this rectangle of screen space. Omit for the whole screen.
    pub region: Option<RegionParam>,
    /// Longest edge of the returned image in pixels. Omit to get screen space itself.
    pub max_edge: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickParams {
    pub x: u32,
    pub y: u32,
    /// left (default), middle or right.
    pub button: Option<Button>,
    /// 1 (default), 2 for a double click, 3 for a triple click.
    pub count: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DragParams {
    pub from: Point,
    pub to: Point,
    /// left (default), middle or right.
    pub button: Option<Button>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScrollParams {
    /// Where to scroll, in screen space.
    pub x: u32,
    pub y: u32,
    /// Wheel notches to the right (negative: left).
    #[serde(default)]
    pub dx: i32,
    /// Wheel notches down (negative: up).
    #[serde(default)]
    pub dy: i32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeTextParams {
    /// The text to type. `\n` presses Return and `\t` presses Tab.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// A key or combo, such as `enter`, `cmd+shift+g`, `ctrl+alt+delete`, `f5` or `cmd++`.
    pub combo: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    /// Only watch this rectangle of screen space. Omit to watch the whole screen.
    pub region: Option<RegionParam>,
    /// How long to wait, in milliseconds (default 5000, at most 60000).
    pub timeout_ms: Option<u64>,
    /// After a change, how long the screen must stay still before returning, in milliseconds
    /// (default 300; 0 returns at the first change).
    pub settle_ms: Option<u64>,
}

/// The MCP server.
#[derive(Clone)]
pub struct Peephole {
    session: Arc<Session>,
    max_edge: u32,
    tool_router: ToolRouter<Self>,
}

fn reply(result: Res<CallToolResult>) -> CallToolResult {
    // Failures go back to the agent as tool errors it can read and act on, not protocol errors.
    result.unwrap_or_else(|e| CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]))
}

fn done(message: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(message.into())])
}

#[tool_router]
impl Peephole {
    /// A server over `session`, with screen space's long edge at `max_edge`.
    #[must_use]
    pub fn new(session: Arc<Session>, max_edge: u32) -> Self {
        let mut tool_router = Self::tool_router();
        if session.input().is_none() {
            for name in INPUT_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self { session, max_edge, tool_router }
    }

    #[tool(description = "Take a PNG screenshot of the remote screen, or zoom into a region of it. \
        The text part says how image pixels map to screen space, and flags a black frame (an asleep display).")]
    async fn screenshot(&self, Parameters(params): Parameters<ScreenshotParams>) -> CallToolResult {
        reply(self.screenshot_impl(params).await)
    }

    #[tool(description = "Report the connection state, the framebuffer size, screen space and its scale.")]
    async fn screen_info(&self) -> CallToolResult {
        self.screen_info_impl()
    }

    #[tool(description = "Wait until the screen (or a region of it) changes and then settles, instead of polling screenshots. \
        Returns when the change happened and when it settled, or that nothing changed before the timeout.")]
    async fn wait_for_change(&self, Parameters(params): Parameters<WaitParams>) -> CallToolResult {
        reply(self.wait_impl(params).await)
    }

    #[tool(description = "Click at a point in screen space.")]
    async fn click(&self, Parameters(params): Parameters<ClickParams>) -> CallToolResult {
        reply(self.click_impl(params).await)
    }

    #[tool(name = "move", description = "Move the pointer to a point in screen space without clicking (for hover menus and tooltips).")]
    async fn move_pointer(&self, Parameters(params): Parameters<Point>) -> CallToolResult {
        reply(self.move_impl(params).await)
    }

    #[tool(description = "Press at `from`, move to `to` and release, all in screen space.")]
    async fn drag(&self, Parameters(params): Parameters<DragParams>) -> CallToolResult {
        reply(self.drag_impl(params).await)
    }

    #[tool(description = "Scroll with the mouse wheel at a point in screen space: dy notches down (negative: up), dx notches right (negative: left).")]
    async fn scroll(&self, Parameters(params): Parameters<ScrollParams>) -> CallToolResult {
        reply(self.scroll_impl(params).await)
    }

    #[tool(description = "Type text into whatever has focus. Newlines press Return and tabs press Tab.")]
    async fn type_text(&self, Parameters(params): Parameters<TypeTextParams>) -> CallToolResult {
        reply(self.type_impl(params).await)
    }

    #[tool(description = "Press a key or combo: `enter`, `esc`, `cmd+shift+g`, `ctrl+alt+delete`, `f5`, `cmd++`. \
        Modifiers: shift, ctrl, alt/option, cmd/super (Command on a Mac), meta.")]
    async fn key(&self, Parameters(params): Parameters<KeyParams>) -> CallToolResult {
        reply(self.key_impl(params).await)
    }
}

impl Peephole {
    fn space(&self, fb: &Framebuffer) -> ScreenSpace {
        ScreenSpace::new(fb.width(), fb.height(), self.max_edge)
    }

    /// The input channel and the framebuffer pixel under each screen-space point, once connected.
    async fn targets<const N: usize>(&self, points: [Point; N]) -> Res<(&Input, [(u16, u16); N])> {
        let input = self.session.input().context("peephole is running --view-only, so input tools are disabled")?;
        self.session.ready(READY_WAIT).await?;
        let fb = self.session.framebuffer();
        let space = self.space(&fb);
        let mut mapped = [(0, 0); N];
        for (out, point) in mapped.iter_mut().zip(points) {
            *out = space.to_framebuffer(point.x, point.y)?;
        }
        Ok((input, mapped))
    }

    async fn screenshot_impl(&self, params: ScreenshotParams) -> Res<CallToolResult> {
        self.session.ready(READY_WAIT).await?;
        let max_edge = params.max_edge.unwrap_or(self.max_edge);
        ensure!(MAX_EDGE_RANGE.contains(&max_edge), "max_edge must be between {} and {}", MAX_EDGE_RANGE.start(), MAX_EDGE_RANGE.end());
        let region = params.region.map(Region::from);
        let space_edge = self.max_edge;
        let (shot, space, fb_size) = self
            .session
            .inspect(move |fb| {
                let space = ScreenSpace::new(fb.width(), fb.height(), space_edge);
                render(fb, &space, region, max_edge).map(|shot| (shot, space, (fb.width(), fb.height())))
            })
            .await??;
        Ok(CallToolResult::success(vec![
            ContentBlock::image(BASE64.encode(&shot.png), "image/png"),
            ContentBlock::text(describe(&shot, &space, fb_size)),
        ]))
    }

    fn screen_info_impl(&self) -> CallToolResult {
        let state = self.session.state();
        let fb = self.session.framebuffer();
        let space = self.space(&fb);
        let mut lines = vec![format!("state: {state}")];
        if let State::Connected(desktop) = &state {
            lines.push(format!("desktop: {:?}", desktop.name));
            lines.push(format!("auth: {}", security::name(desktop.security)));
        }
        if fb.width() > 0 {
            lines.push(format!("framebuffer: {}x{}", fb.width(), fb.height()));
            lines.push(format!("screen space: {}x{} (1 screen px = {:.3} framebuffer px)", space.width, space.height, space.scale));
        }
        let input = if self.session.input().is_some() { "enabled" } else { "disabled (--view-only)" };
        lines.push(format!("input: {input}"));
        done(lines.join("\n"))
    }

    async fn wait_impl(&self, params: WaitParams) -> Res<CallToolResult> {
        self.session.ready(READY_WAIT).await?;
        let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS);
        let area = {
            let fb = self.session.framebuffer();
            match params.region {
                Some(region) => self.space(&fb).to_framebuffer_rect(&region.into())?,
                None => fb.bounds(),
            }
        };
        let settle_ms = params.settle_ms.unwrap_or(DEFAULT_SETTLE_MS).min(MAX_SETTLE_MS);
        let change = self.session.wait_for_change(area, Duration::from_millis(timeout_ms), Duration::from_millis(settle_ms)).await?;
        Ok(done(match change {
            Change::None => format!("no change within {timeout_ms} ms"),
            Change::Settled { first, .. } if settle_ms == 0 => format!("changed after {} ms", first.as_millis()),
            Change::Settled { first, last } => format!("changed after {} ms and settled after {} ms", first.as_millis(), last.as_millis()),
            Change::Unsettled { first } => format!("changed after {} ms and was still changing at the {timeout_ms} ms timeout", first.as_millis()),
        }))
    }

    async fn click_impl(&self, params: ClickParams) -> Res<CallToolResult> {
        let count = params.count.unwrap_or(1);
        ensure!((1..=3).contains(&count), "count must be 1, 2 or 3");
        let button = params.button.unwrap_or_default();
        let (input, [(x, y)]) = self.targets([Point { x: params.x, y: params.y }]).await?;
        input.pointer(x, y, 0).await?;
        for i in 0..count {
            if i > 0 {
                sleep(CLICK_GAP).await;
            }
            input.pointer(x, y, button.mask()).await?;
            sleep(CLICK_HOLD).await;
            input.pointer(x, y, 0).await?;
        }
        Ok(done(format!("clicked {button:?} x{count} at ({}, {})", params.x, params.y).to_lowercase()))
    }

    async fn move_impl(&self, params: Point) -> Res<CallToolResult> {
        let (input, [(x, y)]) = self.targets([params]).await?;
        input.pointer(x, y, 0).await?;
        Ok(done(format!("moved to ({}, {})", params.x, params.y)))
    }

    async fn drag_impl(&self, params: DragParams) -> Res<CallToolResult> {
        let mask = params.button.unwrap_or_default().mask();
        let (input, [from, to]) = self.targets([params.from, params.to]).await?;
        input.pointer(from.0, from.1, 0).await?;
        input.pointer(from.0, from.1, mask).await?;
        for step in 1..=DRAG_STEPS {
            sleep(DRAG_STEP).await;
            let (x, y) = (lerp(from.0, to.0, step, DRAG_STEPS), lerp(from.1, to.1, step, DRAG_STEPS));
            input.pointer(x, y, mask).await?;
        }
        input.pointer(to.0, to.1, 0).await?;
        Ok(done(format!("dragged from ({}, {}) to ({}, {})", params.from.x, params.from.y, params.to.x, params.to.y)))
    }

    async fn scroll_impl(&self, params: ScrollParams) -> Res<CallToolResult> {
        ensure!(params.dx != 0 || params.dy != 0, "nothing to scroll: set dx or dy");
        ensure!(
            params.dx.unsigned_abs() <= MAX_SCROLL && params.dy.unsigned_abs() <= MAX_SCROLL,
            "scroll at most {MAX_SCROLL} notches at a time"
        );
        let (input, [(x, y)]) = self.targets([Point { x: params.x, y: params.y }]).await?;
        input.pointer(x, y, 0).await?;
        let vertical = if params.dy > 0 { buttons::WHEEL_DOWN } else { buttons::WHEEL_UP };
        let horizontal = if params.dx > 0 { buttons::WHEEL_RIGHT } else { buttons::WHEEL_LEFT };
        for (wheel, notches) in [(vertical, params.dy.unsigned_abs()), (horizontal, params.dx.unsigned_abs())] {
            for _ in 0..notches {
                input.pointer(x, y, wheel).await?;
                input.pointer(x, y, 0).await?;
            }
        }
        Ok(done(format!("scrolled dx={} dy={} at ({}, {})", params.dx, params.dy, params.x, params.y)))
    }

    async fn type_impl(&self, params: TypeTextParams) -> Res<CallToolResult> {
        let chars = params.text.chars().count();
        ensure!(chars <= MAX_TEXT, "type at most {MAX_TEXT} characters at a time");
        // Parse everything before sending anything, so bad input never half-types.
        let strokes = text_strokes(&params.text)?;
        let (input, []) = self.targets([]).await?;
        for pair in strokes.as_chunks::<2>().0 {
            send_strokes(input, pair).await?;
            sleep(KEY_GAP).await;
        }
        Ok(done(format!("typed {chars} characters")))
    }

    async fn key_impl(&self, params: KeyParams) -> Res<CallToolResult> {
        let combo = Combo::parse(&params.combo)?;
        let (input, []) = self.targets([]).await?;
        send_strokes(input, &combo.strokes()).await?;
        Ok(done(format!("pressed {}", params.combo.trim())))
    }
}

async fn send_strokes(input: &Input, strokes: &[Stroke]) -> Void {
    for &(keysym, down) in strokes {
        input.key(keysym, down).await?;
    }
    Ok(())
}

/// The point `step` of `steps` along the way from `a` to `b`.
fn lerp(a: u16, b: u16, step: u32, steps: u32) -> u16 {
    let (a, b) = (i64::from(a), i64::from(b));
    let value = a + (b - a) * i64::from(step) / i64::from(steps.max(1));
    u16::try_from(value).unwrap_or(if b > a { u16::MAX } else { 0 })
}

/// The text that travels with a screenshot: what it shows, and how to turn its pixels into
/// screen-space coordinates.
fn describe(shot: &Shot, space: &ScreenSpace, (fb_width, fb_height): (u16, u16)) -> String {
    let r = shot.region;
    let ratio = shot.screen_per_image_px();
    let whole = r.x == 0 && r.y == 0 && r.width == space.width && r.height == space.height;
    let mut text = if whole && shot.width == space.width && shot.height == space.height {
        format!("Screenshot {}x{} of the whole screen, in screen space: click at image coordinates.", shot.width, shot.height)
    } else if whole {
        format!(
            "Screenshot {}x{} of the whole {}x{} screen. One image pixel is {ratio:.3} screen pixels: multiply image coordinates by {ratio:.3} before clicking.",
            shot.width, shot.height, space.width, space.height
        )
    } else {
        format!(
            "Zoomed screenshot {}x{} of screen region x={} y={} width={} height={}. Screen x = {} + image x * {ratio:.3}; screen y = {} + image y * {ratio:.3}.",
            shot.width, shot.height, r.x, r.y, r.width, r.height, r.x, r.y
        )
    };
    let _ = write!(
        text,
        " Screen space is {}x{}; one screen pixel is {:.3} framebuffer pixels ({fb_width}x{fb_height}).",
        space.width, space.height, space.scale
    );
    if shot.black {
        text.push_str(" The frame is entirely black: the display may be asleep. A key press or a click usually wakes it.");
    }
    text
}

// The macro's default router is `Self::tool_router()`, rebuilt per call, which would bring back
// the input tools that `--view-only` removed; route through our own copy instead.
#[tool_handler(router = self.tool_router)]
// The macro generates trait methods that are `async` without awaiting.
#[allow(clippy::unused_async_trait_impl)]
impl ServerHandler for Peephole {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("peephole", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Connects in the background and serves MCP on stdin/stdout until the client hangs up.
///
/// # Errors
///
/// When the MCP transport fails.
pub async fn serve(config: SessionConfig, max_edge: u32) -> Void {
    let session = Arc::new(Session::spawn(config));
    let service = Peephole::new(session, max_edge).serve(rmcp::transport::stdio()).await.context("starting the MCP server")?;
    service.waiting().await.context("serving MCP")?;
    Ok(())
}

#[cfg(test)]
mod unit {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn drags_interpolate_both_ways() {
        assert_eq!(lerp(0, 120, 6, 12), 60);
        assert_eq!(lerp(120, 0, 6, 12), 60);
        assert_eq!(lerp(5, 5, 3, 12), 5);
        assert_eq!(lerp(0, 120, 12, 12), 120);
    }

    fn shot(width: u32, height: u32, region: Region, black: bool) -> Shot {
        Shot {
            png: Vec::new(),
            width,
            height,
            region,
            black,
        }
    }

    #[test]
    fn describes_each_kind_of_screenshot() {
        let space = ScreenSpace::new(3136, 2028, 1568);
        let whole = Region { x: 0, y: 0, width: 1568, height: 1014 };

        let full = describe(&shot(1568, 1014, whole, false), &space, (3136, 2028));
        assert!(full.starts_with("Screenshot 1568x1014 of the whole screen, in screen space"), "{full}");
        assert!(full.contains("2.000 framebuffer pixels"), "{full}");

        let small = describe(&shot(784, 507, whole, false), &space, (3136, 2028));
        assert!(small.contains("multiply image coordinates by 2.000"), "{small}");

        let zoom = describe(&shot(400, 200, Region { x: 100, y: 50, width: 200, height: 100 }, true), &space, (3136, 2028));
        assert!(zoom.contains("Screen x = 100 + image x * 0.500"), "{zoom}");
        assert!(zoom.contains("entirely black"), "{zoom}");
    }
}
