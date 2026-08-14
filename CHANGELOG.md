# Changelog

All notable changes to this project are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Media that needs a downscale — every GIF and video, and the reason the transfer
  plane exists — failed to decode with `media_decoder_failed` on an ordinary
  multi-core host. The decoder's address-space ceiling was 1 GiB, which is below
  what FFmpeg reserves before it decodes anything: worker threads are sized from
  the host's visible CPU count and glibc reserves a 64 MiB arena per thread.
  Across a 3600-fold range of source pixels the requirement moves by single-digit
  percent, while one visible core to eight nearly doubles it, so the ceiling was
  refusing FFmpeg's startup rather than the enormous declared frame it exists to
  refuse. Scaled sources failed first because the scale filter is what adds the
  threads, which is why panel-sized stills kept working. The default is now 2 GiB,
  sized for an eight-core host and still far below a frame beyond
  `max_source_dimension`.

### Added

- `MATRIX_DECODER_ADDRESS_SPACE_MB` sets the decoder's address-space ceiling. The
  requirement follows the host rather than the media, so a machine with
  substantially more than eight cores may need it raised; the configuration and
  deployment documents cover the symptom and how to size it.

## [0.4.0] - 2026-08-13

### Added

- A governed transfer plane, off by default and enabled by setting
  `MATRIX_FILE_PUBLIC_URL` to the public origin this server is reached at.
  Media past the 16 KiB inline cap — GIFs and video, which by definition need a
  downscale — now reaches the panel without passing through a tool argument or
  model context. Bytes are pushed to this server, never fetched by it: a trusted
  intermediary calls `files/authorizeUpload`, receives a single-use descriptor
  pointing at this server's own `PUT /files/upload/{id}` route, streams the media
  there, and a later `matrix_submit_asset` names the staged source. A reference
  and an inline payload use the identical tool contract and return the identical
  report.
- `MATRIX_FILE_PUBLIC_URL` accepts an `http` origin as well as `https`. A
  plaintext origin is sound only where the intermediary already reaches this
  server's `/mcp` in cleartext over a private segment it trusts — it refuses a
  plaintext transfer descriptor from anywhere else. That is a property of the
  deployment's topology which this server cannot observe, so configuring `http`
  is the operator's assertion that it holds, not a check this server performs.
- Settings `MATRIX_FILE_STAGING_DIR`, `MATRIX_FILE_TTL_SECS`, and
  `MATRIX_FILE_MAX_STAGED` bound where transfers are staged, how long an unused
  authorization or unconsumed transfer survives, and how many may be outstanding.
- Refusal codes for the transfer plane: `matrix_file_unauthorized`,
  `matrix_file_too_many_staged`, `matrix_file_declared_too_large`,
  `matrix_file_size_mismatch`, `matrix_file_digest_mismatch`,
  `matrix_file_unsupported_digest`, `matrix_file_staging_failed`, and
  `matrix_file_bad_params`. Every way of failing to present valid transfer
  authority — an unknown identifier, a wrong credential, an expired or already
  used one — answers with the single `matrix_file_unauthorized`, so a caller
  cannot learn which identifiers exist. The operator's log keeps the precise
  reason.

### Changed

- `matrix_submit_asset` accepts `source` as a bare URI string as well as the
  object it has always taken, and publishes both shapes in its schema. SEP-2631
  declares a file-valued tool input as a URI *string* — its file object is the
  shape of outputs and authorization results — so an object-only contract worked
  behind an intermediary that translates it and not for a client speaking the
  draft directly. Both shapes resolve identically, including for a reference the
  transfer plane staged. This widens what is accepted and removes nothing, so
  every existing caller is unaffected; and writing a reference differently does
  not change what a reference may name, so a caller-named URL in either shape is
  still refused with `matrix_unsupported_source`.
- `matrix-media`'s decode entry points take a source handle rather than a byte
  slice, so feeding the probe and the decoder no longer copies the source once per
  subprocess, and a staged transfer streams from disk instead of being held in
  memory. No limit value, refusal code, or ordering changed.

### Security

- Transfer authority is single-use, expiring, minted from the platform CSPRNG,
  stored hashed, compared in constant time, and carried in a request header rather
  than a URL. A transfer is bounded by what was declared and verified against what
  arrived — byte count and SHA-256 — before it can be decoded, and the existing
  `Limits` ceilings still apply. A caller-named URL of any scheme is still refused
  with `matrix_unsupported_source`, including while the plane is configured; no
  code path fetches a destination a caller chose.

## [0.3.0] - 2026-08-09

### Added

- Layout scroll cadence: a `matrix_show_text_layout` scroll region accepts
  `repeat: true` to re-enter for as many evenly spaced crossings as fit the
  package instead of crossing once and parking, and `phase` (0 to below 1,
  requires `repeat`) to offset where in its cycle the region starts. The package
  length and frame budget are unchanged; regions with different cycle lengths and
  phases drift against each other, so a composition animates continuously and
  appears to loop independently. Refusals carry the new `matrix_layout_bad_phase`
  code.
- stdio transport: `matrix-server --stdio` (or `MATRIX_STDIO=1`) serves MCP over
  stdin/stdout for the single client that spawned the process, for MCP clients
  without remote-server support. The tool catalog is identical to the Streamable
  HTTP transport; `MATRIX_HTTP_ADDR` and `MATRIX_ALLOWED_HOSTS` are ignored in this
  mode, and combining `--stdio` with `--healthcheck` is rejected at startup.

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

[Unreleased]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/chrisbennight/mcp-matrix-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/chrisbennight/mcp-matrix-rs/releases/tag/v0.1.0
