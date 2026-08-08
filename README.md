# mcp-matrix-rs

`mcp-matrix-rs` turns a WLED-driven RGB matrix into an MCP display. It renders text
and small inline media into fixed-rate RGB frames, applies a panel power budget, and
streams the result over DDP.

The verified target is the Apollo Automation M-1: a 64x64 HUB75 panel running WLED.
Other WLED RGB matrices may work, but the electrical model is calibrated for that
hardware; see [Compatibility](docs/compatibility.md) before using another panel.

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

## Configuration and support

- [Configuration reference](docs/configuration.md)
- [Deployment and security](docs/deployment.md)
- [Hardware and platform compatibility](docs/compatibility.md)
- [Design decisions](DECISIONS.md)
- [Roadmap and architecture](PLAN.md)

## Development

The workspace uses the toolchain pinned in `rust-toolchain.toml`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker build -t mcp-matrix-rs:local .
```

Unit and integration tests use loopback fakes and never contact a real panel. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the repository workflow.

Version tags publish matching versioned images to `ghcr.io/bennight/mcp-matrix-rs`
(`v1.2.3` publishes `:1.2.3`). No mutable `latest` alias is published. The example
Compose file builds the checked-out revision locally.

## License

MIT
