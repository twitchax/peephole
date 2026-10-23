//! peephole: an MCP server that lets an agent see and drive a remote desktop over VNC, including
//! macOS Screen Sharing with Apple's own login.

pub mod base;
pub mod credentials;
pub mod framebuffer;
pub mod keys;
pub mod mcp;
pub mod rfb;
pub mod screenshot;
pub mod session;
pub mod tests;
