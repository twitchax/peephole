//! The connection. A background task dials the server when a tool first needs it, runs the
//! handshake, and keeps the local framebuffer current while tools are in use; tools read that
//! cache rather than wait on the network for pixels, except for the first look after our own
//! input (see [`Session::freshen`]) or after an idle pause (see [`Session::ready`]).
//!
//! Per connection, a reader applies each `FramebufferUpdate` atomically and bumps a frame counter,
//! and a writer sends input and keeps one incremental update request outstanding (paced, so a
//! busy screen cannot flood us) until no tool has called for [`Idle::pause`]. After
//! [`Idle::disconnect`] the connection closes until the next tool. A failed connection is retried
//! with backoff, except after an authentication failure: retrying a wrong password could lock the
//! account.

use std::{
    convert::Infallible,
    fmt,
    pin::pin,
    sync::{
        Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, anyhow, bail, ensure};
use tokio::{
    io::{AsyncWriteExt as _, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tracing::{info, warn};

use crate::base::{Res, Void};
use crate::credentials::Credentials;
use crate::framebuffer::{Framebuffer, Rect};
use crate::rfb::{
    AuthError, ClientMessage, MessageReader, PixelFormat, ServerMessage, Version, handshake,
    proto::{encoding, security},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// At most ten update requests a second: plenty for an agent, and a busy screen (a video) cannot
/// saturate the link.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Keepalive probes start after this much silence, so a vanished peer is noticed in about a
/// minute and a half instead of hours.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const READ_BUFFER: usize = 256 * 1024;

/// Where to connect, and as whom.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub credentials: Credentials,
    /// No [`Input`] exists at all, so nothing can send a key or pointer event.
    pub view_only: bool,
    pub idle: Idle,
}

/// What a session does when no tool is using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle {
    /// Stop asking for updates after this long without a tool. A session that kept asking would
    /// pull a playing video, or a screensaver, ten frames a second for as long as it lived.
    pub pause: Duration,
    /// Close the connection after this long without a tool; the next one reconnects.
    pub disconnect: Duration,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            pause: Duration::from_secs(30),
            disconnect: Duration::from_mins(10),
        }
    }
}

/// What the connected server told us about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop {
    pub name: String,
    pub version: Version,
    /// The server is macOS Screen Sharing.
    pub apple: bool,
    pub security: u8,
}

/// The connection's state, as tools report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Not connected, because no tool has needed the server lately.
    Idle,
    Connecting,
    /// Connected, with at least one full frame in the framebuffer.
    Connected(Desktop),
    Retrying {
        error: String,
        retry_in: Duration,
    },
    /// Stopped for good, usually because the login was refused.
    Failed(String),
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle (connects at the next tool call)"),
            Self::Connecting => f.write_str("connecting"),
            Self::Connected(desktop) => write!(f, "connected to {:?} (RFB {}, {})", desktop.name, desktop.version, security::name(desktop.security)),
            Self::Retrying { error, retry_in } => write!(f, "disconnected ({error}); retrying in {}s", retry_in.as_secs_f32().ceil()),
            Self::Failed(error) => write!(f, "stopped: {error}"),
        }
    }
}

/// What [`Session::wait_for_change`] saw, timed from the start of the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing changed before the timeout.
    None,
    /// The region changed, and its last change was followed by the full settle period.
    Settled { first: Duration, last: Duration },
    /// The region was still changing when the timeout came.
    Unsettled { first: Duration },
}

/// What tools queue for the writer. One queue keeps a refresh behind the input it follows.
#[derive(Debug)]
enum Outgoing {
    Message(ClientMessage),
    Refresh(Refresh),
}

/// A request for `area` outright. `done` fires once an update that covers it has landed.
#[derive(Debug)]
struct Refresh {
    area: Rect,
    done: oneshot::Sender<()>,
}

/// Queues `outgoing` for the writer.
async fn queue(tx: &mpsc::Sender<Outgoing>, outgoing: Outgoing) -> Void {
    tx.send(outgoing).await.map_err(|_| anyhow!("the session has stopped"))
}

/// How tools are using the session, so it can stop streaming, and let go of the server, when
/// nobody is.
#[derive(Debug, Clone, Copy, Default)]
struct Activity {
    /// When a tool last arrived or finished: `None` before the first.
    used: Option<Instant>,
    /// Tools that need the update stream until they finish.
    busy: usize,
    /// The writer has stopped asking for updates, so the cache may be behind the screen.
    paused: bool,
}

impl Activity {
    /// Whether no tool has used the session for `period`.
    fn idle_for(&self, period: Duration) -> bool {
        self.busy == 0 && self.used.is_none_or(|used| used.elapsed() >= period)
    }
}

/// Keeps the update stream going while a tool waits on it.
struct Busy<'a>(&'a watch::Sender<Activity>);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|a| {
            a.busy -= 1;
            a.used = Some(Instant::now());
        });
    }
}

/// Sends input to the server. Only a session that is not view-only has one.
#[derive(Debug, Clone)]
pub struct Input {
    tx: mpsc::Sender<Outgoing>,
    /// Input messages queued so far.
    sent: Arc<AtomicU64>,
}

impl Input {
    /// Moves the pointer to framebuffer pixel (`x`, `y`) with `buttons` held.
    ///
    /// # Errors
    ///
    /// When the session task has stopped.
    pub async fn pointer(&self, x: u16, y: u16, buttons: u8) -> Void {
        self.send(ClientMessage::Pointer { x, y, buttons }).await
    }

    /// Presses or releases the key `keysym`.
    ///
    /// # Errors
    ///
    /// When the session task has stopped.
    pub async fn key(&self, keysym: u32, down: bool) -> Void {
        self.send(ClientMessage::Key { keysym, down }).await
    }

    async fn send(&self, message: ClientMessage) -> Void {
        self.sent.fetch_add(1, Ordering::AcqRel);
        queue(&self.tx, Outgoing::Message(message)).await
    }
}

/// A handle on the background connection. Dropping it stops the connection.
pub struct Session {
    framebuffer: Arc<RwLock<Framebuffer>>,
    frames: watch::Receiver<u64>,
    state: watch::Receiver<State>,
    activity: Arc<watch::Sender<Activity>>,
    /// Refreshes go here; input goes through `input`, on the same queue.
    outgoing: mpsc::Sender<Outgoing>,
    input: Option<Input>,
    /// `Input::sent` as of the last refresh of the whole frame.
    fresh: AtomicU64,
    task: JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Session {
    /// Starts connecting in the background.
    #[must_use]
    pub fn spawn(config: SessionConfig) -> Self {
        let framebuffer = Arc::new(RwLock::new(Framebuffer::default()));
        let (frames_tx, frames) = watch::channel(0);
        let (state_tx, state) = watch::channel(State::Idle);
        let activity = Arc::new(watch::Sender::new(Activity::default()));
        let (outgoing, outgoing_rx) = mpsc::channel(256);
        let input = (!config.view_only).then(|| Input {
            tx: outgoing.clone(),
            sent: Arc::default(),
        });
        let shared = Shared {
            framebuffer: framebuffer.clone(),
            frames: frames_tx,
            state: state_tx,
            activity: activity.clone(),
            refreshes: Mutex::default(),
        };
        let task = tokio::spawn(run(config, shared, outgoing_rx));
        Self {
            framebuffer,
            frames,
            state,
            activity,
            outgoing,
            input,
            fresh: AtomicU64::new(0),
            task,
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }

    /// The input channel, unless the session is view-only.
    #[must_use]
    pub fn input(&self) -> Option<&Input> {
        self.input.as_ref()
    }

    /// The current frame. Hold it briefly: the reader waits for it to paint the next update.
    pub fn framebuffer(&self) -> RwLockReadGuard<'_, Framebuffer> {
        read(&self.framebuffer)
    }

    /// Runs `f` on the current frame on a blocking thread, for work too heavy for the async
    /// runtime (scaling and encoding a screenshot).
    ///
    /// # Errors
    ///
    /// When the blocking task panics.
    pub async fn inspect<R: Send + 'static>(&self, f: impl FnOnce(&Framebuffer) -> R + Send + 'static) -> Res<R> {
        let framebuffer = self.framebuffer.clone();
        tokio::task::spawn_blocking(move || f(&read(&framebuffer))).await.context("the screenshot task failed")
    }

    /// Waits up to `wait` for a connection with a current frame, connecting an idle session.
    /// Every tool comes through here first, which is what keeps the session awake.
    ///
    /// While no tool was calling, the writer may have stopped asking for updates and the cache
    /// fallen behind the screen; then the whole frame is fetched outright before this returns.
    ///
    /// # Errors
    ///
    /// When the session has failed for good, or has no current frame after `wait`.
    pub async fn ready(&self, wait: Duration) -> Res<Desktop> {
        let deadline = Instant::now() + wait;
        self.activity.send_modify(|a| a.used = Some(Instant::now()));
        let mut state = self.state.clone();
        let desktop = match timeout_at(deadline, state.wait_for(|s| matches!(s, State::Connected(_) | State::Failed(_)))).await {
            Ok(Ok(state)) => match &*state {
                State::Connected(desktop) => desktop.clone(),
                other => bail!("not connected: {other}"),
            },
            Ok(Err(_)) => bail!("the session has stopped"),
            Err(_) => bail!("not connected after {}s: {}", wait.as_secs(), self.state()),
        };
        if self.activity.borrow().paused {
            let bounds = self.framebuffer().bounds();
            let caught_up = self.refresh(bounds, deadline.saturating_duration_since(Instant::now())).await?;
            ensure!(caught_up, "the server sent no fresh frame within {}s", wait.as_secs());
        }
        Ok(desktop)
    }

    /// Makes sure `area` of the cache is no older than our last input, waiting up to `wait`.
    ///
    /// The incremental stream can lag well behind the screen: while a menu is open, macOS Screen
    /// Sharing sends a thin band of it about once a second and the rest only once the menu
    /// closes. So when input has gone out since the whole frame was last refreshed, `area` is
    /// requested outright, which the server answers from a fresh capture. Returns false when that
    /// did not land in time, leaving the cached pixels.
    ///
    /// # Errors
    ///
    /// When the session stops.
    pub async fn freshen(&self, area: Rect, wait: Duration) -> Res<bool> {
        if self.stale() { self.refresh(area, wait).await } else { Ok(true) }
    }

    /// Whether input has gone out since the whole frame was last refreshed.
    fn stale(&self) -> bool {
        self.input.as_ref().is_some_and(|input| input.sent.load(Ordering::Acquire) > self.fresh.load(Ordering::Acquire))
    }

    /// Requests `area` outright, after any input already queued, and waits up to `wait` for it.
    async fn refresh(&self, area: Rect, wait: Duration) -> Res<bool> {
        let sent = self.input.as_ref().map_or(0, |input| input.sent.load(Ordering::Acquire));
        let (done, landed) = oneshot::channel();
        queue(&self.outgoing, Outgoing::Refresh(Refresh { area, done })).await?;
        let landed = matches!(timeout(wait, landed).await, Ok(Ok(())));
        if landed && area == self.framebuffer().bounds() {
            self.fresh.fetch_max(sent, Ordering::AcqRel);
        }
        Ok(landed)
    }

    /// Waits up to `wait` for a pixel in `area` to change, then until `area` has been still for
    /// `settle`, so the caller sees a finished frame rather than the first step of an animation
    /// (a menu bar item highlights before its menu draws). A zero `settle` returns at the first
    /// change.
    ///
    /// After our own input, a quiet spell may only mean the server is holding pixels back (see
    /// [`Self::freshen`]), so each apparent settle is checked against a fresh copy of `area`
    /// before it is reported.
    ///
    /// # Errors
    ///
    /// When `area` lies outside the framebuffer, or the session stops.
    pub async fn wait_for_change(&self, area: Rect, wait: Duration, settle: Duration) -> Res<Change> {
        let _busy = self.busy();
        let start = Instant::now();
        let deadline = start + wait;
        let verify = self.stale() && !settle.is_zero();
        let mut frames = self.frames.clone();
        // Mark the current frame seen before taking the baseline, so no frame slips between.
        frames.borrow_and_update();
        let mut last = self.framebuffer().crop(&area)?;
        // When the region first and most recently changed, and whether a fresh copy has confirmed
        // the latest change is the last.
        let mut changes: Option<(Instant, Instant)> = None;
        let mut verified = false;
        loop {
            let until = changes.map_or(deadline, |(_, latest)| (latest + settle).min(deadline));
            match timeout_at(until, frames.changed()).await {
                Ok(Err(_)) => bail!("the session has stopped"),
                Ok(Ok(())) => {
                    let fb = self.framebuffer();
                    if !fb.matches(&area, &last) {
                        // After a resize the region may not fit; an empty baseline never matches.
                        last = fb.crop(&area).unwrap_or_default();
                        let now = Instant::now();
                        changes = Some(changes.map_or((now, now), |(first, _)| (first, now)));
                        verified = false;
                    }
                }
                Err(_) => {
                    let now = Instant::now();
                    if verify && changes.is_some() && !verified && now < deadline {
                        // The refreshed frame bumps `frames`, so the loop compares it like any other.
                        verified = true;
                        self.refresh(area, deadline - now).await?;
                        continue;
                    }
                    return Ok(match changes {
                        None => Change::None,
                        Some((first, latest)) if now >= latest + settle => Change::Settled {
                            first: first - start,
                            last: latest - start,
                        },
                        Some((first, _)) => Change::Unsettled { first: first - start },
                    });
                }
            }
        }
    }

    /// Marks a tool that needs the update stream until the guard drops.
    fn busy(&self) -> Busy<'_> {
        self.activity.send_modify(|a| {
            a.busy += 1;
            a.used = Some(Instant::now());
        });
        Busy(&self.activity)
    }
}

/// What the background task publishes.
struct Shared {
    framebuffer: Arc<RwLock<Framebuffer>>,
    frames: watch::Sender<u64>,
    state: watch::Sender<State>,
    activity: Arc<watch::Sender<Activity>>,
    /// Refreshes on the wire, waiting for an update that covers them.
    refreshes: Mutex<Vec<Refresh>>,
}

/// A poisoned lock means a reader panicked mid-paint; the pixels are still pixels, so carry on.
fn read(lock: &RwLock<Framebuffer>) -> RwLockReadGuard<'_, Framebuffer> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(lock: &RwLock<Framebuffer>) -> RwLockWriteGuard<'_, Framebuffer> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// 0.5 s, doubling to 30 s.
fn backoff(failures: u32) -> Duration {
    (Duration::from_millis(500) * 2u32.saturating_pow(failures.min(6))).min(MAX_BACKOFF)
}

async fn run(config: SessionConfig, shared: Shared, mut outgoing: mpsc::Receiver<Outgoing>) {
    let mut failures = 0;
    loop {
        if shared.activity.borrow().idle_for(config.idle.disconnect) {
            // Connect only for a tool: a session nobody uses never touches the server.
            shared.state.send_replace(State::Idle);
            let mut activity = shared.activity.subscribe();
            discarding(&mut outgoing, async {
                let _ = activity.wait_for(|a| !a.idle_for(config.idle.disconnect)).await;
            })
            .await;
            failures = 0;
        }
        shared.state.send_replace(State::Connecting);
        let mut connected = false;
        let error = match connection(&config, &shared, &mut outgoing, &mut connected).await {
            Ok(()) => {
                info!(after = ?config.idle.disconnect, "disconnected: no tool has called");
                continue;
            }
            Err(error) => error,
        };
        let message = format!("{error:#}");

        if error.is::<AuthError>() {
            warn!(error = %message, "giving up");
            shared.state.send_replace(State::Failed(message));
            return;
        }
        if connected {
            failures = 0;
        }
        let retry_in = backoff(failures);
        failures += 1;
        warn!(error = %message, retry_in = ?retry_in, "disconnected");
        shared.state.send_replace(State::Retrying { error: message, retry_in });
        discarding(&mut outgoing, sleep_until(Instant::now() + retry_in)).await;
    }
}

/// Runs `until`, dropping whatever tools queue meanwhile: replaying a stale click after
/// reconnecting could land on whatever is on screen by then. A dropped refresh tells its waiter
/// it failed.
async fn discarding(outgoing: &mut mpsc::Receiver<Outgoing>, until: impl Future<Output = ()>) {
    let mut until = pin!(until);
    loop {
        tokio::select! {
            () = &mut until => return,
            _ = next_outgoing(outgoing) => {}
        }
    }
}

/// The next queued input or refresh.
async fn next_outgoing(outgoing: &mut mpsc::Receiver<Outgoing>) -> Outgoing {
    match outgoing.recv().await {
        Some(next) => next,
        // Only once the `Session` is gone, which stops this task anyway.
        None => std::future::pending().await,
    }
}

/// Resolves once no tool has used the session for `period`.
async fn until_idle(activity: &watch::Sender<Activity>, period: Duration) {
    let mut seen = activity.subscribe();
    loop {
        let current = *seen.borrow_and_update();
        if current.idle_for(period) {
            return;
        }
        // A busy tool says when it finishes; otherwise look again when the period would be up. A
        // period past what the clock can hold never ends.
        match current.used.filter(|_| current.busy == 0).and_then(|used| used.checked_add(period)) {
            Some(up) => {
                let _ = timeout_at(up, seen.changed()).await;
            }
            None => {
                let _ = seen.changed().await;
            }
        }
    }
}

/// One connection, from dial to failure, or `Ok` once no tool has used it for
/// [`Idle::disconnect`].
async fn connection(config: &SessionConfig, shared: &Shared, outgoing: &mut mpsc::Receiver<Outgoing>, connected: &mut bool) -> Res<()> {
    // A new connection streams from its first frame.
    shared.activity.send_if_modified(|a| {
        a.paused = false;
        false
    });
    let (host, port) = (config.host.as_str(), config.port);
    let mut tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("timed out connecting to {host}:{port}"))?
        .with_context(|| format!("cannot connect to {host}:{port}"))?;
    tcp.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE_IDLE).with_interval(KEEPALIVE_INTERVAL);
    socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive)?;

    let done = timeout(HANDSHAKE_TIMEOUT, handshake(&mut tcp, &config.credentials))
        .await
        .map_err(|_| anyhow!("timed out in the RFB handshake with {host}:{port}"))??;
    let (width, height) = (done.init.width, done.init.height);
    info!(name = %done.init.name, width, height, version = %done.version, security = %security::name(done.security), "connected");
    *write(&shared.framebuffer) = Framebuffer::new(width, height);

    let (rd, mut wr) = tcp.into_split();
    let mut out = Vec::with_capacity(64);
    for message in [
        ClientMessage::SetPixelFormat(PixelFormat::RGBX),
        ClientMessage::SetEncodings(encoding::PREFERRED),
        ClientMessage::UpdateRequest {
            incremental: false,
            area: Rect::new(0, 0, width, height),
        },
    ] {
        message.encode(&mut out);
    }
    wr.write_all(&out).await?;

    let desktop = Desktop {
        name: done.init.name,
        version: done.version,
        apple: done.apple,
        security: done.security,
    };
    let (updated_tx, updated_rx) = mpsc::unbounded_channel();
    let mut rd = BufReader::with_capacity(READ_BUFFER, rd);
    tokio::select! {
        result = read_loop(&mut rd, shared, desktop, &updated_tx, connected) => result.map(|never| match never {}),
        result = write_loop(&mut wr, shared, outgoing, updated_rx, config.idle.pause) => result.map(|never| match never {}),
        () = until_idle(&shared.activity, config.idle.disconnect) => {
            // Before the socket closes, so no tool takes this connection for a live one.
            shared.state.send_replace(State::Idle);
            Ok(())
        }
    }
}

/// The reader tells the writer each update has landed, and the desktop's current size.
struct Updated {
    resized: bool,
    width: u16,
    height: u16,
}

async fn read_loop(rd: &mut BufReader<OwnedReadHalf>, shared: &Shared, desktop: Desktop, updated: &mpsc::UnboundedSender<Updated>, connected: &mut bool) -> Res<Infallible> {
    let mut reader = MessageReader::new();
    loop {
        let size = {
            let fb = read(&shared.framebuffer);
            (fb.width(), fb.height())
        };
        if reader.read(rd, size).await? != ServerMessage::FramebufferUpdate {
            continue;
        }
        let (applied, width, height) = {
            let mut fb = write(&shared.framebuffer);
            let applied = reader.apply(&mut fb)?;
            (applied, fb.width(), fb.height())
        };
        shared.frames.send_modify(|frame| *frame += 1);
        {
            let mut refreshes = shared.refreshes.lock().unwrap_or_else(PoisonError::into_inner);
            for refresh in refreshes.extract_if(.., |r| r.done.is_closed() || r.area.is_covered_by(&applied.damage)) {
                // A waiter that has given up is fine.
                let _ = refresh.done.send(());
            }
        }
        if !*connected {
            *connected = true;
            shared.state.send_replace(State::Connected(desktop.clone()));
        }
        updated.send(Updated { resized: applied.resized, width, height }).map_err(|_| anyhow!("the writer stopped"))?;
    }
}

async fn write_loop(wr: &mut OwnedWriteHalf, shared: &Shared, outgoing: &mut mpsc::Receiver<Outgoing>, mut updated: mpsc::UnboundedReceiver<Updated>, pause: Duration) -> Res<Infallible> {
    let mut out = Vec::with_capacity(64);
    let mut request: Option<ClientMessage> = None;
    let mut next_request = Instant::now();
    loop {
        out.clear();
        tokio::select! {
            done = updated.recv() => {
                let done = done.context("the reader stopped")?;
                // Ask again only while tools are calling. `Session::ready` catches up on the
                // next one, and that refresh's update starts the stream again.
                let mut paused = false;
                shared.activity.send_if_modified(|a| {
                    a.paused = a.idle_for(pause);
                    paused = a.paused;
                    false
                });
                // After a resize, ask for the whole new desktop.
                request = (!paused).then(|| ClientMessage::UpdateRequest {
                    incremental: !done.resized,
                    area: Rect::new(0, 0, done.width, done.height),
                });
                continue;
            }
            next = next_outgoing(outgoing) => match next {
                Outgoing::Message(message) => message.encode(&mut out),
                Outgoing::Refresh(refresh) => {
                    // A non-incremental request makes the server send the whole area. An update
                    // already in flight that happens to cover it also counts: a one-round-trip
                    // window, and RFB has no way to tell the two apart without the Fence extension.
                    ClientMessage::UpdateRequest { incremental: false, area: refresh.area }.encode(&mut out);
                    shared.refreshes.lock().unwrap_or_else(PoisonError::into_inner).push(refresh);
                }
            },
            () = sleep_until(next_request), if request.is_some() => {
                if let Some(message) = request.take() {
                    message.encode(&mut out);
                }
                next_request = Instant::now() + MIN_UPDATE_INTERVAL;
            }
        }
        wr.write_all(&out).await.context("writing to the server")?;
    }
}

#[cfg(test)]
mod unit {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn backoff_doubles_up_to_a_cap() {
        let delays: Vec<u64> = (0..9).map(|n| backoff(n).as_millis().try_into().unwrap_or(u64::MAX)).collect();
        assert_eq!(delays, [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
    }

    #[test]
    fn states_read_well() {
        let desktop = Desktop {
            name: "Mac".into(),
            version: Version::V3_8,
            apple: true,
            security: security::ARD,
        };
        assert_eq!(State::Connected(desktop).to_string(), "connected to \"Mac\" (RFB 3.8, Apple login (30))");
        assert_eq!(
            State::Retrying {
                error: "connection reset".into(),
                retry_in: Duration::from_millis(1500)
            }
            .to_string(),
            "disconnected (connection reset); retrying in 2s"
        );
    }
}
