# mcp-matrix-rs

[![test](https://github.com/chrisbennight/mcp-matrix-rs/actions/workflows/test.yml/badge.svg)](https://github.com/chrisbennight/mcp-matrix-rs/actions/workflows/test.yml)
[![image](https://github.com/chrisbennight/mcp-matrix-rs/actions/workflows/image.yml/badge.svg)](https://github.com/chrisbennight/mcp-matrix-rs/actions/workflows/image.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![release](https://img.shields.io/github/v/release/chrisbennight/mcp-matrix-rs)](https://github.com/chrisbennight/mcp-matrix-rs/releases)

`mcp-matrix-rs` turns a WLED-driven RGB matrix into an MCP display. It renders text
and small inline media into fixed-rate RGB frames, applies a panel power budget, and
streams the result over DDP.

The verified target is the Apollo Automation M-1: a 64x64 HUB75 panel running WLED.
Other WLED RGB matrices may work, but the electrical model is calibrated for that
hardware; see [Compatibility](docs/compatibility.md) before using another panel.

## Why

WLED can already run built-in effects, and tools like xLights or LedFx can stream pixels to it. What none of that provides is a safe way for an AI agent to put arbitrary content on the panel: something has to rasterize text, decode untrusted media inside hard limits, pace frames to the rate the panel actually achieves, and keep the output inside the panel's power budget.

`mcp-matrix-rs` is that layer. It gives any MCP client a physical display it can drive with a tool call, while the server owns normalization, rate adaptation, and power clamping. The caller never touches raw frames or the device.

## What works

- MCP `2026-07-28` over Streamable HTTP at `/mcp`
- centered and scrolling text; input is capped at 100 characters, and long marquees
  must also fit the configured frame budget
- PNG, GIF, and video normalization through isolated FFmpeg subprocesses
- fixed-rate playout with feedback from WLED's reported frame rate
- software power clamping before frames reach the panel
- power, brightness, playback, asset, status, and device tools

Media submitted through MCP is limited to a 16 KiB base64 `data:` URI. Larger-media
transfer is planned; text and native-resolution still images fit within this limit.

## Status and non-goals

This is an early release, verified on one panel model, and maintained as a personal project. Review [PLAN.md](PLAN.md) before depending on behaviour that is not in a tagged release.

Deliberate non-goals: the server does not authenticate clients (operators own that boundary), does not fetch caller-supplied URLs, and does not claim support for panels beyond the verified hardware. The reasoning is recorded in [DECISIONS.md](DECISIONS.md).

## Quick start

You need Docker, Docker Compose, and a WLED device reachable from the Docker host by
HTTP and UDP. Configure WLED to accept DDP on UDP port 4048, retain a bounded realtime
timeout, and set an appropriate power ceiling before sending frames.

```sh
cp .env.example .env
# Edit .env and replace the documentation-only panel address.
docker compose -f compose.example.yml up --build
curl --fail http://127.0.0.1:8080/healthz
```

Connect an MCP client to `http://127.0.0.1:8080/mcp`, then call
`matrix_describe_device` before displaying content. The example publishes the service
on loopback only.

The server has no built-in authentication. Do not expose it directly to the internet
or an untrusted LAN. See [Deployment and security](docs/deployment.md) before changing
the listener or published-port address.

## Usage

Point any MCP client that speaks Streamable HTTP at `http://127.0.0.1:8080/mcp`.
For Claude Code:

```sh
claude mcp add --transport http matrix http://127.0.0.1:8080/mcp
```

or in a client's JSON configuration:

```json
{
  "mcpServers": {
    "matrix": { "type": "http", "url": "http://127.0.0.1:8080/mcp" }
  }
}
```

A typical session from the client:

1. `matrix_describe_device` — confirm the panel's identity, dimensions, and power
   headroom before displaying content.
2. `matrix_show_text` with `{"text": "HELLO"}` — text that fits shows as a centered
   still; longer text scrolls as a marquee until stopped.
3. `matrix_submit_asset` with a base64 `data:` URI (16 KiB limit), then
   `matrix_play` with the returned asset handle.
4. `matrix_stop` — the panel returns to its configured ambient behaviour.

### Tools

| Tool | Purpose |
| --- | --- |
| `matrix_describe_device` | Panel identity, firmware, dimensions, achieved framerate, and power draw |
| `matrix_status` | What is playing, how many assets are held, last reported framerate |
| `matrix_submit_asset` | Normalize media into frames and hold it; returns an asset handle |
| `matrix_list_assets` | List the assets currently held |
| `matrix_play` | Play a held asset, replacing whatever was playing |
| `matrix_show_text` | Centered still or scrolling marquee text |
| `matrix_stop` | Stop playback; the panel returns to ambient behaviour |
| `matrix_set_brightness` | Set panel brightness, 0 to 255 |
| `matrix_power` | Turn the panel on or off |

## Configuration and support

- [Configuration reference](docs/configuration.md)
- [Deployment and security](docs/deployment.md)
- [Hardware and platform compatibility](docs/compatibility.md)
- [Design decisions](DECISIONS.md)
- [Roadmap and architecture](PLAN.md)

## Development

The workspace uses the toolchain pinned in `rust-toolchain.toml`.

Current crate organization/purpose:

| Crate | Purpose |
| --- | --- |
| `matrix-frame` | Canvas geometry and the fixed-rate frame representation; no I/O or device knowledge |
| `matrix-device` | WLED JSON client for configuration and state, DDP transport for frames |
| `matrix-media` | Untrusted media decode and normalization, out of process under a deadline and limits |
| `matrix-playout` | Rate adaptation, power clamping, and paced sends; the only frame-send path |
| `matrix-text` | Rasterizing text into the shared frame representation |
| `matrix-server` | The binary: MCP transport, tool dispatch, and shared engine state |

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker build -t mcp-matrix-rs:local .
```

Unit and integration tests use loopback fakes and never contact a real panel. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the repository workflow.

Version tags publish matching versioned images to `ghcr.io/chrisbennight/mcp-matrix-rs`
(`v1.2.3` publishes `:1.2.3`). No mutable `latest` alias is published. The example
Compose file builds the checked-out revision locally.

## License

MIT
