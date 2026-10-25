//! Opening an RFB session: version, security, ClientInit/ServerInit (RFC 6143 §7.1–7.3).

use std::fmt;

use anyhow::{Context as _, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::auth::{self, ArdChallenge};
use super::proto::{PixelFormat, security};
use crate::base::{Res, Void};
use crate::credentials::{Credentials, PASSWORD_ENV, USER_ENV};

/// Longest server reason or desktop name we read; anything longer is a broken server.
const MAX_STRING: u32 = 64 * 1024;
/// How macOS Screen Sharing greets, and no other server we know of.
const APPLE_GREETING: &[u8; 12] = b"RFB 003.889\n";

/// The protocol versions we speak. Anything newer, like Apple's `003.889`, is answered as 3.8,
/// exactly as libvncclient does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V3_3,
    V3_7,
    V3_8,
}

impl Version {
    fn negotiate(greeting: &[u8; 12]) -> Res<Self> {
        let text = String::from_utf8_lossy(greeting);
        let parse = |digits: &[u8]| std::str::from_utf8(digits).ok().and_then(|d| d.parse::<u32>().ok());
        let (Some(major), Some(minor)) = (parse(&greeting[4..7]), parse(&greeting[8..11])) else {
            bail!("not an RFB server (greeting {:?})", text.trim_end());
        };
        ensure!(
            greeting.starts_with(b"RFB ") && greeting[7] == b'.' && greeting[11] == b'\n',
            "not an RFB server (greeting {:?})",
            text.trim_end()
        );
        Ok(match (major, minor) {
            (3, 8..) | (4.., _) => Self::V3_8,
            (3, 7) => Self::V3_7,
            // RFC 6143 §7.1.1: treat any other 3.x as 3.3.
            (3, _) => Self::V3_3,
            _ => bail!("unsupported RFB version {major}.{minor}"),
        })
    }

    fn greeting(self) -> &'static [u8; 12] {
        match self {
            Self::V3_3 => b"RFB 003.003\n",
            Self::V3_7 => b"RFB 003.007\n",
            Self::V3_8 => b"RFB 003.008\n",
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::V3_3 => "3.3",
            Self::V3_7 => "3.7",
            Self::V3_8 => "3.8",
        })
    }
}

/// A failure that retrying cannot fix: the server rejected the login, or it wants credentials we
/// do not have. The session stops instead of retrying, which could lock the account.
#[derive(Debug)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

/// The server's description of the desktop (RFC 6143 §7.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub pixel_format: PixelFormat,
    pub name: String,
}

/// The outcome of a successful handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub version: Version,
    /// The server greeted as macOS Screen Sharing does.
    pub apple: bool,
    pub security: u8,
    pub init: ServerInit,
}

/// Runs the handshake over `stream` and leaves it ready for normal messages.
///
/// # Errors
///
/// On I/O failure or a malformed server, and with an [`AuthError`] when the login is refused or no
/// supplied credential fits what the server offers.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, credentials: &Credentials) -> Res<Handshake> {
    let mut greeting = [0u8; 12];
    stream.read_exact(&mut greeting).await.context("reading the server's RFB greeting")?;
    let version = Version::negotiate(&greeting)?;
    stream.write_all(version.greeting()).await?;

    let security = choose_security(stream, version, credentials).await?;
    authenticate(stream, version, security, credentials).await?;

    // ClientInit: shared, so we never kick another viewer (or the person at the Mac) off.
    stream.write_all(&[1]).await?;
    let init = read_server_init(stream).await.context("reading ServerInit")?;
    Ok(Handshake {
        version,
        apple: &greeting == APPLE_GREETING,
        security,
        init,
    })
}

/// Reads what the server offers and picks the strongest type our credentials can satisfy.
async fn choose_security<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, version: Version, credentials: &Credentials) -> Res<u8> {
    if version == Version::V3_3 {
        // 3.3 servers choose for us.
        let chosen = stream.read_u32().await.context("reading the security type")?;
        if chosen == u32::from(security::INVALID) {
            bail!("the server refused the connection: {}", read_string(stream).await?);
        }
        return u8::try_from(chosen)
            .ok()
            .filter(|t| [security::NONE, security::VNC].contains(t))
            .ok_or_else(|| anyhow::anyhow!("unsupported security type {chosen}"));
    }

    let count = stream.read_u8().await.context("reading the security types")?;
    if count == 0 {
        bail!("the server refused the connection: {}", read_string(stream).await?);
    }
    let mut offered = vec![0u8; usize::from(count)];
    stream.read_exact(&mut offered).await?;
    let chosen = pick_security(&offered, credentials)?;
    stream.write_all(&[chosen]).await?;
    Ok(chosen)
}

/// Apple's login when we have a username and password, then the VNC password, then None.
fn pick_security(offered: &[u8], credentials: &Credentials) -> Res<u8> {
    let has_user = credentials.username.is_some();
    let has_password = credentials.password.is_some();
    let preference = [(security::ARD, has_user && has_password), (security::VNC, has_password), (security::NONE, true)];
    if let Some((chosen, _)) = preference.iter().find(|(t, usable)| *usable && offered.contains(t)) {
        return Ok(*chosen);
    }

    let names = offered.iter().map(|t| security::name(*t)).collect::<Vec<_>>().join(", ");
    let hint = if offered.contains(&security::ARD) {
        format!("set {USER_ENV} and {PASSWORD_ENV} (or --password-file) to the Mac's login")
    } else if offered.contains(&security::VNC) {
        format!("set {PASSWORD_ENV} (or --password-file) to the VNC password")
    } else {
        "peephole supports None, VNC password and Apple login".to_owned()
    };
    Err(AuthError(format!("the server offers {names}; {hint}")).into())
}

async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, version: Version, security: u8, credentials: &Credentials) -> Void {
    match security {
        security::NONE => {
            // Only 3.8 sends a SecurityResult after None.
            if version == Version::V3_8 {
                read_security_result(stream, version).await?;
            }
        }
        security::VNC => {
            let password = credentials.password.as_ref().ok_or_else(|| AuthError(format!("the server wants a VNC password; set {PASSWORD_ENV}")))?;
            let mut challenge = [0u8; 16];
            stream.read_exact(&mut challenge).await.context("reading the VNC auth challenge")?;
            let response = auth::vnc_response(password.expose().as_bytes(), &challenge);
            stream.write_all(&response).await?;
            read_security_result(stream, version).await?;
        }
        security::ARD => {
            let (Some(username), Some(password)) = (&credentials.username, &credentials.password) else {
                return Err(AuthError(format!("Apple's login needs {USER_ENV} and {PASSWORD_ENV}")).into());
            };
            let challenge = read_ard_challenge(stream).await.context("reading the ARD key exchange")?;
            let response = auth::ard_response(&challenge, username, password.expose()).map_err(|e| AuthError(format!("{e:#}")))?;
            stream.write_all(&response.ciphertext).await?;
            stream.write_all(&response.public_key).await?;
            read_security_result(stream, version).await?;
        }
        other => bail!("unsupported security type {other}"),
    }
    Ok(())
}

async fn read_ard_challenge<S: AsyncRead + Unpin>(stream: &mut S) -> Res<ArdChallenge> {
    let generator = stream.read_u16().await?;
    let key_len = usize::from(stream.read_u16().await?);
    // Bound the allocation before trusting the length.
    ensure!(auth::ARD_KEY_LEN.contains(&key_len), "the server's ARD key length ({key_len} bytes) is out of range");
    let mut prime = vec![0u8; key_len];
    let mut server_public = vec![0u8; key_len];
    stream.read_exact(&mut prime).await?;
    stream.read_exact(&mut server_public).await?;
    Ok(ArdChallenge { generator, prime, server_public })
}

async fn read_security_result<S: AsyncRead + Unpin>(stream: &mut S, version: Version) -> Void {
    let result = stream.read_u32().await.context("reading the security result")?;
    if result == 0 {
        return Ok(());
    }
    // Only 3.8 explains a failure.
    let reason = if version == Version::V3_8 { read_string(stream).await.unwrap_or_default() } else { String::new() };
    let detail = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
    Err(AuthError(format!(
        "the server rejected the login{detail}; check the username, the password, and that this user may use Screen Sharing"
    ))
    .into())
}

async fn read_server_init<S: AsyncRead + Unpin>(stream: &mut S) -> Res<ServerInit> {
    let width = stream.read_u16().await?;
    let height = stream.read_u16().await?;
    let mut format = [0u8; 16];
    stream.read_exact(&mut format).await?;
    let name = read_string(stream).await?;
    Ok(ServerInit {
        width,
        height,
        pixel_format: PixelFormat::from_bytes(&format),
        name,
    })
}

/// A u32-length-prefixed string, decoded leniently (servers send Latin-1 and UTF-8 alike).
async fn read_string<S: AsyncRead + Unpin>(stream: &mut S) -> Res<String> {
    let len = stream.read_u32().await?;
    ensure!(len <= MAX_STRING, "the server sent a {len}-byte string");
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use pretty_assertions::assert_eq;
    use tokio::io::duplex;

    use super::*;
    use crate::credentials::Secret;

    fn creds(user: Option<&str>, password: Option<&str>) -> Credentials {
        Credentials {
            username: user.map(str::to_owned),
            password: password.map(|p| Secret::new(p.to_owned())),
        }
    }

    #[test]
    fn negotiates_versions_like_libvncclient() {
        assert_eq!(Version::negotiate(b"RFB 003.889\n").unwrap(), Version::V3_8);
        assert_eq!(Version::negotiate(b"RFB 003.008\n").unwrap(), Version::V3_8);
        assert_eq!(Version::negotiate(b"RFB 003.007\n").unwrap(), Version::V3_7);
        assert_eq!(Version::negotiate(b"RFB 003.005\n").unwrap(), Version::V3_3);
        assert!(Version::negotiate(b"SSH-2.0-xyz\n").is_err());
        assert!(Version::negotiate(b"RFB 002.000\n").is_err());
    }

    #[test]
    fn prefers_apples_login_when_it_has_a_username() {
        assert_eq!(pick_security(&[2, 30], &creds(Some("u"), Some("p"))).unwrap(), 30);
        assert_eq!(pick_security(&[2, 30], &creds(None, Some("p"))).unwrap(), 2);
        assert_eq!(pick_security(&[1, 2], &creds(None, None)).unwrap(), 1);
        assert_eq!(pick_security(&[1, 2], &creds(None, Some("p"))).unwrap(), 2);
    }

    #[test]
    fn explains_missing_credentials_without_retrying() {
        let error = pick_security(&[30, 35], &creds(None, Some("hunter2"))).unwrap_err();
        assert!(error.is::<AuthError>());
        let message = error.to_string();
        assert!(message.contains(USER_ENV), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
    }

    /// A scripted 3.8 server offering None, ending in a `ServerInit`.
    #[tokio::test]
    async fn completes_a_none_handshake() {
        let (mut client, mut server) = duplex(1024);
        let server = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0u8; 12];
            server.read_exact(&mut version).await.unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            server.write_all(&[1, security::NONE]).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), security::NONE);
            server.write_u32(0).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), 1, "shared flag");
            server.write_all(&[0, 64, 0, 32]).await.unwrap();
            server.write_all(&PixelFormat::RGBX.to_bytes()).await.unwrap();
            server.write_u32(4).await.unwrap();
            server.write_all(b"desk").await.unwrap();
        });

        let done = handshake(&mut client, &Credentials::default()).await.unwrap();
        server.await.unwrap();
        assert_eq!(done.version, Version::V3_8);
        assert!(!done.apple, "only Apple's 003.889 greeting marks macOS");
        assert_eq!(done.security, security::NONE);
        assert_eq!((done.init.width, done.init.height, done.init.name.as_str()), (64, 32, "desk"));
    }

    #[tokio::test]
    async fn reports_a_refused_connection_with_the_servers_reason() {
        let (mut client, mut server) = duplex(1024);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0u8; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[0, 0, 0, 0, 9]).await.unwrap();
            server.write_all(b"too many!").await.unwrap();
        });
        let error = handshake(&mut client, &Credentials::default()).await.unwrap_err();
        assert!(error.to_string().contains("too many!"), "{error:#}");
    }
}
