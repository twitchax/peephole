//! A real RFB server for wire-level tests: `rustvncserver`, reachable on a loopback port. For
//! behaviour rustvncserver cannot show (a server that holds pixels back), see [`stalling`].
//!
//! rustvncserver can only listen on `0.0.0.0:<fixed port>`, with no way to pass in a listener or
//! read back an ephemeral port, which races between parallel tests. Its reverse-connect API dials
//! out instead, so each client that connects to our own loopback listener is spliced to a reverse
//! connection the server makes into a second loopback listener.
//!
//! Two rustvncserver 2.2.1 bugs shape what these tests can cover:
//! - `rfb_encodings::translate::pixel_formats_equal` tests `!true_colour_flag != 0` (a bitwise
//!   NOT on a `u8`, always true), so it never compares shifts and sends every 32bpp format in its
//!   native RGBA order. Our RGBX request matches that order, so frames still decode correctly.
//! - Each client's message loop holds that client's write lock for the whole connection, and
//!   `do_copy_rect` waits on a read lock for every client, so `CopyRect` cannot be driven while a
//!   client is connected. `CopyRect` is covered by the decoder's unit tests instead.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

pub mod stalling;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use rustvncserver::{VncServer, server::ServerEvent};
use tokio::{
    net::TcpListener,
    sync::mpsc::UnboundedReceiver,
    time::{Instant, timeout},
};

/// Generous: a loaded CI runner can be slow, and a hang is caught by nextest's own timeout.
pub const WAIT: Duration = Duration::from_secs(10);

pub struct TestServer {
    pub addr: SocketAddr,
    pub server: Arc<VncServer>,
    pub events: UnboundedReceiver<ServerEvent>,
}

impl TestServer {
    /// Starts a `width` x `height` server, with VNC auth when `password` is set.
    pub async fn start(width: u16, height: u16, password: Option<&str>) -> Self {
        let (server, events) = VncServer::new(width, height, "peephole-test".to_owned(), password.map(str::to_owned));
        let server = Arc::new(server);
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let back = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = front.local_addr().unwrap();
        let back_port = back.local_addr().unwrap().port();

        let dialer = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = front.accept().await else { return };
                let reverse = dialer.clone();
                // Resolves only after the handshake, which flows through the splice below.
                tokio::spawn(async move { reverse.connect_reverse("127.0.0.1".to_owned(), back_port).await });
                let Ok((mut upstream, _)) = back.accept().await else { return };
                tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });

        Self { addr, server, events }
    }

    /// Fills the server's framebuffer from RGB8 pixels.
    pub async fn show(&self, rgb: &[u8]) {
        let rgba: Vec<u8> = rgb.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect();
        self.server.framebuffer().update_from_slice(&rgba).await.unwrap();
    }

    /// The server's framebuffer as RGB8.
    pub async fn pixels(&self) -> Vec<u8> {
        self.server.framebuffer().get_full_data().await.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect()
    }

    /// The next `n` pointer and key events, in order, skipping connection events.
    pub async fn input_events(&mut self, n: usize) -> Vec<Input> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let event = timeout(deadline.saturating_duration_since(Instant::now()), self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out with {} of {n} input events: {out:?}", out.len()))
                .expect("server event channel closed");
            match event {
                ServerEvent::PointerMove { x, y, button_mask, .. } => out.push(Input::Pointer(x, y, button_mask)),
                ServerEvent::KeyPress { key, down, .. } => out.push(Input::Key(key, down)),
                _ => {}
            }
        }
        out
    }
}

/// An input event as the server saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Pointer(u16, u16, u8),
    Key(u32, bool),
}
