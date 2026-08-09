# PLAN

Architecture, scope, and future work for `mcp-matrix-rs`. The reasoning behind material
design choices lives in [`DECISIONS.md`](DECISIONS.md).

## Goal

An MCP upstream that renders caller-supplied media to a WLED LED matrix in realtime.
Text, stills, GIFs, and video go in; frames go out over DDP. Clients connect directly
over Streamable HTTP or through a trusted intermediary chosen by the operator.

## Architecture

**Two planes, deliberately separated.** WLED's JSON API over HTTP configures the device
— power, brightness, ambient behaviour, and the `leds.fps` and power figures the server
reads back. DDP over UDP carries pixels. Configuration never carries frames, and frames
never carry configuration.

**One internal representation.** Text, stills, GIFs, and video all normalize to a
fixed-rate sequence of full-canvas RGB frames. Source-format knowledge lives only in
ingest; scheduling and playout are format-blind. Variable inter-frame timing — GIF
delays in particular — is resampled away at ingest so playout stays a fixed tick.

**Ingest never touches playout.** Decode is asynchronous, bounded, and isolated. The
frame pump reads only fully resident, fully normalized assets; a partially decoded
asset is refused rather than shown degraded.

**Decode is the untrusted boundary.** Media decoders parse complex caller-supplied
formats. Decode runs out of process under a hard timeout and resource limits, with caps
on source size, duration, dimensions, and normalized output.

**The device is protected in software.** Full-canvas luminance is clamped against the
panel's reported ceiling. A HUB75 panel multiplexes, so full white across 4096 pixels
draws around 3 A — at its rating rather than far past it — and the clamp exists for a
supply derated below that, whether shared, undersized, or configured low. Realtime
mode's bounded timeout means a server failure returns the panel to ambient rather than
freezing a frame.

## Components

- `matrix-frame` owns canvas geometry and the fixed-rate frame representation. It has no
  I/O, async runtime, or device knowledge.
- `matrix-device` owns the WLED JSON client and DDP transport.
- `matrix-media` owns bounded, out-of-process decode and normalization of byte media.
- `matrix-playout` owns rate adaptation, power clamping, and the paced frame-send path.
- `matrix-text` rasterizes strings into the same frame representation without a decoder,
  including multi-region layouts of fixed and scrolling text.
- `matrix-server` owns the binary, MCP transport, tool dispatch, and shared engine state.

The standalone binary binds to loopback by default and serves plaintext HTTP. Operators
that expose it beyond loopback provide authentication and TLS at a trusted boundary.

## Future work

**File plane consumption.** Accepting authorized artifact references instead of only
inline bytes, so larger media reaches this server without passing through model context.
Reference resolution belongs to a trusted transfer boundary; this server must not fetch
a destination selected by an MCP caller.

**Contention arbitration.** Priority, deduplication, duration caps, preemption, and the
scheduler that would apply them. `Playout` drives one sequence at a time and holds it
until the sequence ends or a send budget is reached. No policy currently arbitrates
between callers; its shape should follow observed contention rather than assumptions.

**Dirty-region transmission.** DDP carries an offset and length, so transmitting only
changed spans is available. Whether it is worth building depends on measurements showing
that bandwidth is the binding constraint.

**Playback lifecycle notifications.** Subscriptions and the MCP tasks extension could
represent long-running decode and playback work. Decodes complete in seconds and
playback is observable through `matrix_status`, so the current tool surface remains
synchronous until a consumer needs those protocol features.

## Non-goals

- WLED configuration writes and device reboot. Native WLED effects survive as ambient
  configuration, not as a rendering path.
- Application-layer client authentication. Operators provide the authorization boundary.
- Production deployment configuration. This repository provides only portable examples.
