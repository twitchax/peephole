//! A server that holds pixels back the way macOS Screen Sharing does while a menu is open. A
//! click paints a menu at once, but incremental updates only ever carry a thin highlight, band by
//! band; the menu arrives only for a non-incremental request. It greets with Apple's `003.889`, so
//! peephole takes it for macOS Screen Sharing, and it reports the input it receives.

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use peephole::framebuffer::Rect;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{Instant, sleep_until, timeout},
};

use super::{
    Input, WAIT,
    mock::{Message, Screen, accept, read_message},
};

/// Painted at the first click, and only ever sent outright.
pub const MENU: Rect = Rect::new(0, 16, 96, 64);
pub const MENU_RGB: [u8; 3] = [230, 230, 230];
/// Painted and sent one band at a time after a click, as Apple's server trickles a menu out.
pub const HIGHLIGHT: Rect = Rect::new(0, 0, 80, 8);
pub const HIGHLIGHT_RGB: [u8; 3] = [40, 90, 200];
const BANDS: u16 = 10;
const BAND_EVERY: Duration = Duration::from_millis(100);

pub struct StallingServer {
    pub addr: SocketAddr,
    fetches: Arc<Mutex<Vec<Rect>>>,
    inputs: UnboundedReceiver<Input>,
}

impl StallingServer {
    /// Starts a server showing `rgb` (RGB8, `width` x `height`).
    pub async fn start(width: u16, height: u16, rgb: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let log = fetches.clone();
        let (received, inputs) = unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let screen = Screen { width, height, rgb: rgb.clone() };
                tokio::spawn(serve(stream, screen, log.clone(), received.clone()));
            }
        });
        Self { addr, fetches, inputs }
    }

    /// Every non-incremental update request answered so far, the first full frame included.
    pub fn fetches(&self) -> Vec<Rect> {
        self.fetches.lock().unwrap().clone()
    }

    /// The next `n` input events, in order.
    pub async fn input_events(&mut self, n: usize) -> Vec<Input> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let event = timeout(deadline.saturating_duration_since(Instant::now()), self.inputs.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out with {} of {n} input events", out.len()))
                .expect("input channel closed");
            out.push(event);
        }
        out
    }
}

async fn serve(stream: TcpStream, mut screen: Screen, fetches: Arc<Mutex<Vec<Rect>>>, received: UnboundedSender<Input>) -> io::Result<()> {
    let mut s = BufReader::new(stream);
    accept(&mut s, b"RFB 003.889\n", screen.width, screen.height).await?;

    let mut clicked = false;
    let (mut bands_sent, mut next_band) = (BANDS, Instant::now());
    loop {
        tokio::select! {
            kind = s.read_u8() => match read_message(&mut s, kind?).await? {
                // Incremental requests go unanswered: the bands below are all they ever get.
                Message::Update { incremental: false, area } => {
                    fetches.lock().unwrap().push(area);
                    s.write_all(&screen.update(area)).await?;
                }
                Message::Input(input) => {
                    if matches!(input, Input::Pointer(_, _, buttons) if buttons != 0) && !clicked {
                        clicked = true;
                        screen.paint(MENU, MENU_RGB);
                        (bands_sent, next_band) = (0, Instant::now() + BAND_EVERY);
                    }
                    // A test that never reads them is fine.
                    let _ = received.send(input);
                }
                Message::Update { incremental: true, .. } | Message::Setup => {}
            },
            // Sent unasked: while in use, peephole always has an incremental request outstanding.
            () = sleep_until(next_band), if bands_sent < BANDS => {
                let width = HIGHLIGHT.width / BANDS;
                let band = Rect::new(HIGHLIGHT.x + bands_sent * width, HIGHLIGHT.y, width, HIGHLIGHT.height);
                screen.paint(band, HIGHLIGHT_RGB);
                s.write_all(&screen.update(band)).await?;
                bands_sent += 1;
                next_band += BAND_EVERY;
            }
        }
    }
}
