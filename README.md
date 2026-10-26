# peephole

[![Build and Test](https://github.com/twitchax/peephole/actions/workflows/build.yml/badge.svg)](https://github.com/twitchax/peephole/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/twitchax/peephole/branch/main/graph/badge.svg)](https://codecov.io/gh/twitchax/peephole)
[![Version](https://img.shields.io/crates/v/peephole.svg)](https://crates.io/crates/peephole)
[![Docs](https://docs.rs/peephole/badge.svg)](https://docs.rs/peephole)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

An MCP server that lets an agent see and drive a remote desktop over VNC: take screenshots, click,
drag, scroll, type and press key combos. It works with any VNC server, and with macOS Screen Sharing
using the Mac's own username and password (Apple's login, RFB security type 30), so the legacy
8-character VNC password can stay off.

## Usage

```text
$ peephole -h
An MCP server that lets an agent see and drive a remote desktop over VNC, including macOS Screen Sharing

Usage: peephole [OPTIONS] --host <HOST>

Options:
      --host <HOST>                    The VNC server's host name or address [env: PEEPHOLE_HOST=]
      --port <PORT>                    The VNC server's port [env: PEEPHOLE_PORT=] [default: 5900]
      --password-file <PASSWORD_FILE>  Read the password from this file (trailing newlines ignored) instead of `PEEPHOLE_PASSWORD` [env: PEEPHOLE_PASSWORD_FILE=]
      --view-only                      Disable every input tool, leaving `screenshot`, `screen_info` and `wait_for_change` [env: PEEPHOLE_VIEW_ONLY=]
      --max-edge <MAX_EDGE>            The long edge of screen space (and of a default screenshot), in pixels [env: PEEPHOLE_MAX_EDGE=] [default: 1568]
      --idle-pause <SECS>              Seconds without a tool call before peephole stops asking for screen updates; the next call fetches the whole screen first [env: PEEPHOLE_IDLE_PAUSE=] [default: 30]
      --idle-disconnect <SECS>         Seconds without a tool call before peephole disconnects; the next call reconnects [env: PEEPHOLE_IDLE_DISCONNECT=] [default: 600]
  -h, --help                           Print help (see more with '--help')
  -V, --version                        Print version
```

### With Claude Code

```bash
# A Mac: its login name, with the password in a file only you can read.
claude mcp add peephole -e PEEPHOLE_USER=<mac-user> -- peephole --host <host> --password-file <path>

# Any VNC server with a VNC password.
claude mcp add peephole -e PEEPHOLE_PASSWORD=<vnc-password> -- peephole --host <host>
```

Credentials come only from `PEEPHOLE_USER`, `PEEPHOLE_PASSWORD` or `--password-file`, never from
arguments, and never appear in logs or tool output. `claude mcp add -e` stores the variable in
your Claude Code config, so prefer `--password-file` (mode `600`) for a real password.

Add peephole only where sessions should drive the desktop. `claude mcp add` registers it for the
current directory (local scope); with `-s user`, every Claude Code session starts its own copy.

On the Mac, turn on **System Settings › General › Sharing › Screen Sharing** and allow the user
you log in as. peephole picks Apple's login when `PEEPHOLE_USER` and a password are set, the VNC
password when only a password is set, and no authentication when the server offers none.

### Tools

| Tool | What it does |
|---|---|
| `screenshot { region?, max_edge? }` | A PNG of the screen, or a zoom into `region`. The text part says how image pixels map to screen space and flags a black frame. |
| `click { x, y, button?, count? }` | Click (left, middle or right; double and triple with `count`). |
| `move { x, y }` | Move the pointer without clicking. |
| `drag { from, to, button? }` | Press, move in steps, release. |
| `scroll { x, y, dx?, dy? }` | Wheel notches: `dy` down (negative up), `dx` right (negative left). |
| `type_text { text }` | Type text; `\n` is Return and `\t` is Tab. |
| `key { combo }` | `enter`, `esc`, `cmd+shift+g`, `ctrl+alt+delete`, `f5`, `cmd++`. `cmd` is Command on a Mac. |
| `screen_info` | Connection state, framebuffer size, screen space and scale. |
| `wait_for_change { region?, timeout_ms?, settle_ms? }` | Wait for the screen (or a region) to change and then hold still for `settle_ms` (300 by default; 0 returns at the first change), instead of polling. |

`--view-only` removes every input tool.

**Screen space.** Coordinates are in screen space: the framebuffer scaled so its long edge is
`--max-edge` (1,568 px by default; a 3456x2234 Retina frame becomes 1568x1014). A default
screenshot *is* screen space, so its pixel coordinates can be clicked directly. A `region`
screenshot zooms in, up to the framebuffer's own resolution, without changing screen space.

**Fresh pixels after input.** Screenshots normally come from a local copy that the server keeps
current. macOS Screen Sharing can hold changes back, though: while a menu is open it sends a
thin band of it about once a second, and the rest only once the menu closes. So after any input,
the next `screenshot` asks the server for its area outright (about half a second for a full
Retina frame on a home network), and `wait_for_change` checks a quiet screen against a fresh
copy before it reports the screen settled.

**Scrolling a Mac.** macOS Screen Sharing scrolls about one point per wheel click, where other
servers scroll a line or more. So on a Mac each `scroll` notch sends 16 clicks: about a line of
text.

**Idle.** peephole connects at the first tool call and keeps its copy of the screen current while
tools are in use. After 30 s without a call it stops asking for updates, since a playing video or
a screensaver would otherwise stream at ten frames a second. The next call then fetches the whole
screen first (about half a second for a Retina Mac). After 10 minutes it disconnects, and the
next call reconnects. `--idle-pause` and `--idle-disconnect` set the two periods, in seconds.

**Security.** Apple's login and VNC authentication protect the credentials, not the session:
screen contents and keystrokes cross the network unencrypted, as with any plain VNC client. Use
peephole on a network you trust, or through a tunnel (SSH, WireGuard).

## Install

Prebuilt binaries are attached to each [GitHub Release](https://github.com/twitchax/peephole/releases/latest):

```bash
# Linux
curl -LO https://github.com/twitchax/peephole/releases/latest/download/peephole_x86_64-unknown-linux-gnu.zip
# macOS (Apple silicon)
curl -LO https://github.com/twitchax/peephole/releases/latest/download/peephole_aarch64-apple-darwin.zip
# Windows
curl -LO https://github.com/twitchax/peephole/releases/latest/download/peephole_x86_64-pc-windows-gnu.zip
```

```bash
cargo install peephole
```

## Development

```bash
cargo make ci        # fmt-check + clippy (-D warnings) + nextest: the gate CI runs
cargo make uat       # ci + the #[ignore]d slow suites
cargo make codecov   # coverage.lcov
```

## License

MIT
