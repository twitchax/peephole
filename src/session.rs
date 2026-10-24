//! The long-lived connection. One background task dials the server, runs the handshake, and keeps
//! the local framebuffer current; tools read that cache and never wait on the network for pixels.
//!
//! Per connection, a reader applies each `FramebufferUpdate` atomically and bumps a frame counter,
//! and a writer sends input and keeps one incremental update request outstanding (paced, so a
//! busy screen cannot flood us). The connection is retried with backoff, except after an
//! authentication failure: retrying a wrong password could lock the account.

use std::{
    convert::Infallible,
    fmt,
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::Duration,
};

use anyhow::{Context as _, anyhow, bail};
use tokio::{
    io::{AsyncWriteExt as _, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, watch},
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
    /// No input channel exists at all, so nothing can send a key or pointer event.
    pub view_only: bool,
}

/// What the connected server told us about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop {
    pub name: String,
    pub version: Version,
    pub security: u8,
}

/// The connection's state, as tools report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
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

/// Sends input to the server. Only a session that is not view-only has one.
#[derive(Debug, Clone)]
pub struct Input {
    tx: mpsc::Sender<ClientMessage>,
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
        self.tx.send(message).await.map_err(|_| anyhow!("the session has stopped"))
    }
}

/// A handle on the background connection. Dropping it stops the connection.
pub struct Session {
    framebuffer: Arc<RwLock<Framebuffer>>,
    frames: watch::Receiver<u64>,
    state: watch::Receiver<State>,
    input: Option<Input>,
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
        let (state_tx, state) = watch::channel(State::Connecting);
        let (input, input_rx) = if config.view_only {
            (None, None)
        } else {
            let (tx, rx) = mpsc::channel(256);
            (Some(Input { tx }), Some(rx))
        };
        let shared = Shared {
            framebuffer: framebuffer.clone(),
            frames: frames_tx,
            state: state_tx,
        };
        let task = tokio::spawn(run(config, shared, input_rx));
        Self { framebuffer, frames, state, input, task }
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

    /// Waits up to `wait` for a connection with a frame to show.
    ///
    /// # Errors
    ///
    /// When the session has failed for good, or is still not connected after `wait`.
    pub async fn ready(&self, wait: Duration) -> Res<Desktop> {
        let mut state = self.state.clone();
        let waited = timeout(wait, state.wait_for(|s| matches!(s, State::Connected(_) | State::Failed(_)))).await;
        match waited {
            Ok(Ok(state)) => match &*state {
                State::Connected(desktop) => Ok(desktop.clone()),
                other => bail!("not connected: {other}"),
            },
            Ok(Err(_)) => bail!("the session has stopped"),
            Err(_) => bail!("not connected after {}s: {}", wait.as_secs(), self.state()),
        }
    }

    /// Waits up to `wait` for a pixel in `area` to change, then until `area` has been still for
    /// `settle`, so the caller sees a finished frame rather than the first step of an animation
    /// (a menu bar item highlights before its menu draws). A zero `settle` returns at the first
    /// change.
    ///
    /// # Errors
    ///
    /// When `area` lies outside the framebuffer, or the session stops.
    pub async fn wait_for_change(&self, area: Rect, wait: Duration, settle: Duration) -> Res<Change> {
        let start = Instant::now();
        let deadline = start + wait;
        let mut frames = self.frames.clone();
        // Mark the current frame seen before taking the baseline, so no frame slips between.
        frames.borrow_and_update();
        let mut last = self.framebuffer().crop(&area)?;
        // When the region first and most recently changed.
        let mut changes: Option<(Instant, Instant)> = None;
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
                    }
                }
                Err(_) => {
                    return Ok(match changes {
                        None => Change::None,
                        Some((first, latest)) if Instant::now() >= latest + settle => Change::Settled {
                            first: first - start,
                            last: latest - start,
                        },
                        Some((first, _)) => Change::Unsettled { first: first - start },
                    });
                }
            }
        }
    }
}

/// What the background task publishes.
struct Shared {
    framebuffer: Arc<RwLock<Framebuffer>>,
    frames: watch::Sender<u64>,
    state: watch::Sender<State>,
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

async fn run(config: SessionConfig, shared: Shared, mut input: Option<mpsc::Receiver<ClientMessage>>) {
    let mut failures = 0;
    loop {
        shared.state.send_replace(State::Connecting);
        let mut connected = false;
        let Err(error) = connection(&config, &shared, &mut input, &mut connected).await;
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

        // Drop input sent while disconnected: replaying a stale click after reconnecting could
        // land on whatever is on screen by then.
        let deadline = Instant::now() + retry_in;
        loop {
            tokio::select! {
                () = sleep_until(deadline) => break,
                _ = next_input(&mut input) => {}
            }
        }
    }
}

/// The next queued input. Never resolves for a view-only session.
async fn next_input(input: &mut Option<mpsc::Receiver<ClientMessage>>) -> ClientMessage {
    if let Some(rx) = input
        && let Some(message) = rx.recv().await
    {
        return message;
    }
    std::future::pending().await
}

/// One connection, from dial to failure.
async fn connection(config: &SessionConfig, shared: &Shared, input: &mut Option<mpsc::Receiver<ClientMessage>>, connected: &mut bool) -> Res<Infallible> {
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
        security: done.security,
    };
    let (updated_tx, updated_rx) = mpsc::unbounded_channel();
    let mut rd = BufReader::with_capacity(READ_BUFFER, rd);
    tokio::select! {
        result = read_loop(&mut rd, shared, desktop, &updated_tx, connected) => result,
        result = write_loop(&mut wr, input, updated_rx) => result,
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
        if !*connected {
            *connected = true;
            shared.state.send_replace(State::Connected(desktop.clone()));
        }
        updated.send(Updated { resized: applied.resized, width, height }).map_err(|_| anyhow!("the writer stopped"))?;
    }
}

async fn write_loop(wr: &mut OwnedWriteHalf, input: &mut Option<mpsc::Receiver<ClientMessage>>, mut updated: mpsc::UnboundedReceiver<Updated>) -> Res<Infallible> {
    let mut out = Vec::with_capacity(64);
    let mut request: Option<ClientMessage> = None;
    let mut next_request = Instant::now();
    loop {
        out.clear();
        tokio::select! {
            done = updated.recv() => {
                let done = done.context("the reader stopped")?;
                // After a resize, ask for the whole new desktop.
                request = Some(ClientMessage::UpdateRequest {
                    incremental: !done.resized,
                    area: Rect::new(0, 0, done.width, done.height),
                });
                continue;
            }
            message = next_input(input) => message.encode(&mut out),
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
