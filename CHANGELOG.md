# Changelog

All notable changes to this project are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-08

Initial public release.

### Added

- MCP `2026-07-28` server over Streamable HTTP at `/mcp` with nine tools: device
  description, status, asset submission and listing, playback, text display, stop,
  brightness, and power.
- Centered and scrolling text rendering with a 100-character input cap and a
  configurable frame budget.
- PNG, GIF, and video normalization through isolated FFmpeg subprocesses under a
  deadline and resource ceilings; inline media accepted as a base64 `data:` URI up
  to 16 KiB.
- Fixed-rate DDP playout paced by the framerate the panel reports, with software
  power clamping calibrated for the Apollo Automation M-1.
- Host-header allowlist as a DNS-rebinding defense, loopback-only defaults, and a
  hardened example Compose deployment.
- Versioned container images published to `ghcr.io/chrisbennight/mcp-matrix-rs` on
  release tags.

[Unreleased]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/chrisbennight/mcp-matrix-rs/releases/tag/v0.1.0
