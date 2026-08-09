# Changelog

All notable changes to this project are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- stdio transport: `matrix-server --stdio` (or `MATRIX_STDIO=1`) serves MCP over
  stdin/stdout for the single client that spawned the process, for MCP clients
  without remote-server support. The tool catalog is identical to the Streamable
  HTTP transport; `MATRIX_HTTP_ADDR`, `MATRIX_ALLOWED_HOSTS`, and `--healthcheck`
  do not apply in this mode.

## [0.2.0] - 2026-08-09

### Changed

- Tool parameters now refuse unknown fields instead of silently ignoring them, so a
  typo'd key surfaces as a deserialization error rather than a wrong render. The
  `source` file object keeps accepting unknown keys because its shape follows an
  external draft that may grow fields.

### Added

- `matrix_show_text_layout` tool: composes up to 16 non-overlapping rectangular text
  regions — fixed (left/center/right aligned) or scrolling along four canonical
  paths, each reversible, at a speed in pixels per second capped at the panel's
  frame rate — into one bounded fixed-rate package. The longest scroller sets the
  package length, finished scrollers park outside their destination edge, and
  looping repeats the whole package. Layout refusals carry stable `matrix_layout_*`
  error codes; per-region text problems reuse the existing `matrix_text_*` codes.

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

[Unreleased]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/chrisbennight/mcp-matrix-rs/releases/tag/v0.1.0
