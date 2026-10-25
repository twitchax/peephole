//! A server that holds pixels back the way macOS Screen Sharing does while a menu is open. A
//! click paints a menu at once, but incremental updates only ever carry a thin highlight, band by
//! band; the menu arrives only for a non-incremental request. It greets with Apple's `003.889`, so
//! peephole takes it for macOS Screen Sharing, and it reports the pointer events it receives.

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use peephole::{framebuffer::Rect, rfb::PixelFormat};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{Instant, sleep_until, timeout},
};

use super::{Input, WAIT};

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
    pointers: UnboundedReceiver<Input>,
}

impl StallingServer {
    /// Starts a server showing `rgb` (RGB8, `width` x `height`).
    pub async fn start(width: u16, height: u16, rgb: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let log = fetches.clone();
        let (pointed, pointers) = unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let screen = Screen { width, rgb: rgb.clone() };
                tokio::spawn(serve(stream, height, screen, log.clone(), pointed.clone()));
            }
        });
        Self { addr, fetches, pointers }
    }

    /// Every non-incremental update request answered so far, the first full frame included.
    pub fn fetches(&self) -> Vec<Rect> {
        self.fetches.lock().unwrap().clone()
    }

    /// The next `n` pointer events, in order.
    pub async fn pointer_events(&mut self, n: usize) -> Vec<Input> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let event = timeout(deadline.saturating_duration_since(Instant::now()), self.pointers.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out with {} of {n} pointer events", out.len()))
                .expect("pointer channel closed");
            out.push(event);
        }
        out
    }
}

struct Screen {
    width: u16,
    rgb: Vec<u8>,
}

impl Screen {
    fn paint(&mut self, rect: Rect, colour: [u8; 3]) {
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let at = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 3;
                self.rgb[at..at + 3].copy_from_slice(&colour);
            }
        }
    }

    /// A one-rectangle Raw `FramebufferUpdate` of `rect`, in the RGBX format peephole asks for.
    fn update(&self, rect: Rect) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend(1u16.to_be_bytes());
        for v in [rect.x, rect.y, rect.width, rect.height] {
            out.extend(v.to_be_bytes());
        }
        out.extend(0i32.to_be_bytes());
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let at = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 3;
                out.extend([self.rgb[at], self.rgb[at + 1], self.rgb[at + 2], 0]);
            }
        }
        out
    }
}

async fn serve(stream: TcpStream, height: u16, mut screen: Screen, fetches: Arc<Mutex<Vec<Rect>>>, pointed: UnboundedSender<Input>) -> io::Result<()> {
    let mut s = BufReader::new(stream);
    s.write_all(b"RFB 003.889\n").await?;
    s.read_exact(&mut [0; 12]).await?;
    s.write_all(&[1, 1]).await?; // one security type: None
    s.read_u8().await?;
    s.write_u32(0).await?;
    s.read_u8().await?; // ClientInit
    let name = b"stalling";
    let mut init = Vec::new();
    init.extend(screen.width.to_be_bytes());
    init.extend(height.to_be_bytes());
    init.extend(PixelFormat::RGBX.to_bytes());
    init.extend(u32::try_from(name.len()).unwrap().to_be_bytes());
    init.extend(name);
    s.write_all(&init).await?;

    let mut clicked = false;
    let (mut bands_sent, mut next_band) = (BANDS, Instant::now());
    loop {
        tokio::select! {
            kind = s.read_u8() => match kind? {
                // SetPixelFormat: peephole always asks for RGBX, which is what `update` sends.
                0 => { s.read_exact(&mut [0; 19]).await?; }
                2 => {
                    s.read_u8().await?;
                    let count = s.read_u16().await?;
                    s.read_exact(&mut vec![0; 4 * usize::from(count)]).await?;
                }
                3 => {
                    let incremental = s.read_u8().await? != 0;
                    let area = Rect::new(s.read_u16().await?, s.read_u16().await?, s.read_u16().await?, s.read_u16().await?);
                    // Incremental requests go unanswered: the bands below are all they ever get.
                    if !incremental {
                        fetches.lock().unwrap().push(area);
                        s.write_all(&screen.update(area)).await?;
                    }
                }
                4 => { s.read_exact(&mut [0; 7]).await?; }
                5 => {
                    let buttons = s.read_u8().await?;
                    let (x, y) = (s.read_u16().await?, s.read_u16().await?);
                    // A test that never reads them is fine.
                    let _ = pointed.send(Input::Pointer(x, y, buttons));
                    if buttons != 0 && !clicked {
                        clicked = true;
                        screen.paint(MENU, MENU_RGB);
                        (bands_sent, next_band) = (0, Instant::now() + BAND_EVERY);
                    }
                }
                other => return Err(io::Error::other(format!("unexpected client message {other}"))),
            },
            // Sent unasked: peephole always has an incremental request outstanding anyway.
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
