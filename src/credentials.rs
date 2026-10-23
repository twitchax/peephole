//! Credentials for the RFB handshake. They come only from the environment or from a file the user
//! writes, never from argv (which `ps` shows to every user), and they never reach a log line, an
//! error or tool output: [`Secret`] redacts itself in `Debug` and zeroizes its memory on drop.

use std::{fmt, path::Path};

use anyhow::{Context as _, bail};
use zeroize::Zeroizing;

use crate::base::Res;

/// The login name: the macOS account for Apple's login (security type 30).
pub const USER_ENV: &str = "PEEPHOLE_USER";
/// The password: the macOS account password for type 30, or the VNC password for type 2.
pub const PASSWORD_ENV: &str = "PEEPHOLE_PASSWORD";

/// A secret string, redacted in `Debug` and zeroized on drop.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The secret itself. Only the auth code should call this.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Whatever credentials the user supplied; which ones the handshake uses depends on what the
/// server offers.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: Option<Secret>,
}

impl Credentials {
    /// Reads `PEEPHOLE_USER` and either `PEEPHOLE_PASSWORD` or `password_file`.
    ///
    /// # Errors
    ///
    /// Fails when both a password variable and a password file are given, or the file cannot be
    /// read or is empty.
    pub fn load(password_file: Option<&Path>) -> Res<Self> {
        Self::from_sources(|name| std::env::var(name).ok(), password_file)
    }

    /// [`Credentials::load`] with the environment lookup injected, so tests need not mutate the
    /// process environment.
    ///
    /// # Errors
    ///
    /// As [`Credentials::load`].
    pub fn from_sources(env: impl Fn(&str) -> Option<String>, password_file: Option<&Path>) -> Res<Self> {
        let username = env(USER_ENV).filter(|user| !user.is_empty());
        let env_password = env(PASSWORD_ENV).filter(|password| !password.is_empty()).map(Secret::new);
        let password = match (password_file, env_password) {
            (Some(_), Some(_)) => bail!("both {PASSWORD_ENV} and --password-file are set; use one"),
            (Some(path), None) => Some(read_password_file(path)?),
            (None, password) => password,
        };
        Ok(Self { username, password })
    }
}

/// The whole file is the password, minus trailing line breaks.
fn read_password_file(path: &Path) -> Res<Secret> {
    let mut password = Zeroizing::new(std::fs::read_to_string(path).with_context(|| format!("cannot read password file {}", path.display()))?);
    warn_if_shared(path);
    let len = password.trim_end_matches(['\r', '\n']).len();
    password.truncate(len);
    if password.is_empty() {
        bail!("password file {} is empty", path.display());
    }
    Ok(Secret(password))
}

#[cfg(unix)]
fn warn_if_shared(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "the password file is readable by other users; `chmod 600` it");
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_path: &Path) {}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn reads_user_and_password_from_the_environment() {
        let creds = Credentials::from_sources(env(&[(USER_ENV, "alice"), (PASSWORD_ENV, "hunter2")]), None).unwrap();
        assert_eq!(creds.username.as_deref(), Some("alice"));
        assert_eq!(creds.password.unwrap().expose(), "hunter2");
    }

    #[test]
    fn treats_empty_variables_as_unset() {
        let creds = Credentials::from_sources(env(&[(USER_ENV, ""), (PASSWORD_ENV, "")]), None).unwrap();
        assert!(creds.username.is_none());
        assert!(creds.password.is_none());
    }

    #[test]
    fn reads_the_password_file_without_its_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "correct horse\r\n").unwrap();
        let creds = Credentials::from_sources(env(&[]), Some(&path)).unwrap();
        assert_eq!(creds.password.unwrap().expose(), "correct horse");
    }

    #[test]
    fn rejects_an_empty_password_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "\n").unwrap();
        assert!(Credentials::from_sources(env(&[]), Some(&path)).is_err());
    }

    #[test]
    fn refuses_two_password_sources() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "from-file").unwrap();
        let error = Credentials::from_sources(env(&[(PASSWORD_ENV, "from-env")]), Some(&path)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("use one"), "{message}");
        assert!(!message.contains("from-env") && !message.contains("from-file"), "{message}");
    }

    #[test]
    fn debug_output_never_shows_the_password() {
        let creds = Credentials {
            username: Some("alice".into()),
            password: Some(Secret::new("hunter2".into())),
        };
        let debug = format!("{creds:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }
}
