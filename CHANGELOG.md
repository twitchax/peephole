# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Each release opens with a sentence on
why the version moved the way it did.

## [Unreleased]

### Added

- A stdio MCP server with `screenshot`, `click`, `move`, `drag`, `scroll`, `type_text`, `key`,
  `screen_info` and `wait_for_change`, in a screen space scaled to a 1,568 px long edge.
- An RFB client of our own: Apple's login (security type 30), VNC authentication and None;
  Raw, CopyRect and ZRLE with DesktopSize, ExtendedDesktopSize and LastRect.
- One long-lived connection with reconnect and backoff; a refused login stops it rather than
  retrying.
- Credentials from `PEEPHOLE_USER`, `PEEPHOLE_PASSWORD` or `--password-file` only, never logged.
- `--view-only`, which removes every input tool.
- `wait_for_change` waits for the screen to settle after a change (`settle_ms`, 300 by default),
  so a menu that highlights before it draws is seen whole.
- After input, the next `screenshot` fetches its area fresh, and `wait_for_change` confirms a
  settle against fresh pixels: macOS Screen Sharing holds an open menu back from its incremental
  updates.
- On macOS Screen Sharing, each `scroll` notch sends 16 wheel clicks: Apple's server scrolls
  about a point per click.
