# peephole: project instructions

An MCP server that lets an agent see and drive a remote desktop over VNC, including macOS Screen Sharing

The global conventions in `~/CLAUDE.md` and the `bootstrap-rust-project` skill's `conventions.md`
apply in full. This file records only what is specific to peephole; **where it is silent,
defer to `~/CLAUDE.md`.**

## Architecture

One package: a library (`peephole`) and a thin binary (`peephole`, `src/bin.rs`) behind the
default `cli` feature. The binary parses arguments and calls `mcp::serve`. `base` holds the error
aliases; `tests` is the shared fixture factory.

- `rfb/`: our own RFB client (handshake, `auth` math, `decode`), generic over `AsyncRead + AsyncWrite`.
  We request `PixelFormat::RGBX` (libvncclient's little-endian default), so pixels arrive in
  storage order. Apple's `003.889` is answered as 3.8, as libvncclient does.
- `session`: one background connection. The reader paints whole updates under the framebuffer
  lock and never holds it across a network read. An `AuthError` stops the session for good:
  retrying a wrong password could lock the Mac account.
- `screenshot`: screen space (the framebuffer scaled to `--max-edge`). Every tool coordinate is in
  screen space; a region screenshot zooms without changing it.
- `mcp`: the rmcp tools. `#[tool_handler(router = self.tool_router)]` is load-bearing: the macro's
  default rebuilds the router per call and would restore the input tools `--view-only` removed.
- `credentials`: env or file only, `Secret` redacts and zeroizes. Tests assert no password in any
  output, at trace level.
- Tests: `tests/rfb.rs` and `tests/e2e.rs` run against rustvncserver through
  `tests/support` (read its header for two upstream bugs that shape coverage); `tests/ard.rs` mocks
  Apple's type-30 server.

## Commands

```bash
cargo make ci        # fmt-check + clippy (-D warnings) + nextest: the gate, run verbatim in CI
cargo make test      # nextest
cargo make uat       # ci + the #[ignore]d slow suites: what PRD UATs point at
cargo make codecov   # coverage.lcov
```

## Conventions

- **Toolchain:** pinned in `rust-toolchain.toml` (a dated nightly). Nothing else
  may pin a toolchain; `repo_sync` tests enforce it.
- **Lints:** the `[workspace.lints]` table in `Cargo.toml` (`all` + `pedantic` + `unused` +
  `unwrap_used` / `expect_used` denied). No new `allow`s beyond a narrow, commented one, and never
  for `too_many_*` or `needless_pass_by_value`: refactor instead.
- **Errors:** `anyhow` through the `Res` / `Void` aliases, with `.context()` on every `?` that crosses an I/O or parse boundary.
- **Tests:** unit tests in-module, integration and e2e in `tests/` (one subsystem per file, `e2e_*`
  names for tests that spawn the binary), fixtures from the `tests` factory module. Every
  behavioral change ships a test.
- **PRDs** live in `.prds/`; reference task IDs in commits, and never mark a UAT verified without a
  real, passing test.
- **No AI attribution** anywhere: no `Co-Authored-By` trailers, session links or "generated with"
  footers in commits, PRs, changelogs or docs.
