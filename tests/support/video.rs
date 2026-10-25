//! A server whose screen never stops changing, like a playing video or a screensaver: every
//! incremental update request is answered at once with a newly painted `FRAME`, and nothing else.
//! It logs every update request and tracks connections, for the session's idle behaviour.

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use peephole::framebuffer::Rect;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};

use super::{
    WAIT,
    mock::{Message, Screen, accept, read_message},
};

/// The part of the screen that plays.
pub const FRAME: Rect = Rect::new(0, 0, 8, 8);

/// An update request as the server saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub incremental: bool,
    pub area: Rect,
}

pub struct VideoServer {
    pub addr: SocketAddr,
    screen: Arc<Mutex<Screen>>,
    requests: Arc<watch::Sender<Vec<Request>>>,
    connections: Arc<AtomicUsize>,
    open: Arc<watch::Sender<usize>>,
}

impl VideoServer {
    /// Starts a black `width` x `height` server.
    pub async fn start(width: u16, height: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let screen = Arc::new(Mutex::new(Screen {
            width,
            height,
            rgb: vec![0; usize::from(width) * usize::from(height) * 3],
        }));
        let server = Self {
            addr: listener.local_addr().unwrap(),
            screen,
            requests: Arc::new(watch::Sender::new(Vec::new())),
            connections: Arc::default(),
            open: Arc::new(watch::Sender::new(0)),
        };
        let (screen, requests, connections, open) = (server.screen.clone(), server.requests.clone(), server.connections.clone(), server.open.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::AcqRel);
                open.send_modify(|n| *n += 1);
                let (screen, requests, open) = (screen.clone(), requests.clone(), open.clone());
                tokio::spawn(async move {
                    let _ = serve(stream, &screen, &requests).await;
                    open.send_modify(|n| *n -= 1);
                });
            }
        });
        server
    }

    /// Changes the screen without sending anything: only a non-incremental request shows it.
    pub fn paint(&self, rect: Rect, colour: [u8; 3]) {
        self.screen.lock().unwrap().paint(rect, colour);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.borrow().clone()
    }

    /// The first `n` update requests, once there are that many.
    pub async fn wait_for_requests(&self, n: usize) -> Vec<Request> {
        let mut requests = self.requests.subscribe();
        let all = timeout(WAIT, requests.wait_for(|r| r.len() >= n))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {n} requests: {:?}", self.requests()))
            .unwrap()
            .clone();
        all[..n].to_vec()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Acquire)
    }

    /// Waits until every connection has closed.
    pub async fn closed(&self) {
        let mut open = self.open.subscribe();
        timeout(WAIT, open.wait_for(|&n| n == 0)).await.expect("a connection is still open").unwrap();
    }
}

async fn serve(stream: TcpStream, screen: &Mutex<Screen>, requests: &watch::Sender<Vec<Request>>) -> io::Result<()> {
    let mut s = BufReader::new(stream);
    let (width, height) = {
        let screen = screen.lock().unwrap();
        (screen.width, screen.height)
    };
    accept(&mut s, b"RFB 003.008\n", width, height).await?;
    let mut frame: u8 = 0;
    loop {
        let kind = s.read_u8().await?;
        let Message::Update { incremental, area } = read_message(&mut s, kind).await? else { continue };
        requests.send_modify(|r| r.push(Request { incremental, area }));
        let update = {
            let mut screen = screen.lock().unwrap();
            if incremental {
                frame = frame.wrapping_add(1);
                screen.paint(FRAME, [frame, 255 - frame, 128]);
                screen.update(FRAME)
            } else {
                screen.update(area)
            }
        };
        s.write_all(&update).await?;
    }
}
