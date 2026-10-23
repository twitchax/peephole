# peephole

[![Build and Test](https://github.com/twitchax/peephole/actions/workflows/build.yml/badge.svg)](https://github.com/twitchax/peephole/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/twitchax/peephole/branch/main/graph/badge.svg)](https://codecov.io/gh/twitchax/peephole)
[![Version](https://img.shields.io/crates/v/peephole.svg)](https://crates.io/crates/peephole)
[![Docs](https://docs.rs/peephole/badge.svg)](https://docs.rs/peephole)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

An MCP server that lets an agent see and drive a remote desktop over VNC, including macOS Screen Sharing

## Usage

```text
$ peephole --help
TODO: paste the real `--help` output once the CLI settles.
```

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
