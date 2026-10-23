//! A minimal RFB (VNC) client: just what peephole needs, and Apple's login. Everything is generic
//! over `AsyncRead + AsyncWrite`, so tests can drive it over an in-memory pipe.

pub mod auth;
pub mod decode;
pub mod handshake;
pub mod proto;

pub use decode::{Applied, MessageReader, ServerMessage};
pub use handshake::{AuthError, Handshake, ServerInit, Version, handshake};
pub use proto::{ClientMessage, PixelFormat};
