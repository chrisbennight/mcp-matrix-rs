# Compatibility

## Verified hardware

The supported reference target is the Apollo Automation M-1 with its 64x64 RGB HUB75
panel and WLED firmware. The implementation uses WLED's JSON API for device state and
RGB24 DDP on UDP port 4048 for frames.

The canvas is configurable and the DDP packetizer supports larger frame buffers, but
the following assumptions are not yet generalized:

- The power estimate uses 0.732 mA per pixel at full white, calibrated from the M-1's
  approximately 3 A full-white draw across 4096 multiplexed pixels.
- Frames are RGB24. RGBW strips and receivers requiring another DDP pixel type are not
  supported.
- WLED is the tested DDP receiver and the only JSON API implementation supported.

Do not treat the software clamp as an electrical safety device. Configure WLED's power
limiter for the actual supply and panel, retain appropriate fusing and wiring, and
validate another panel's current model before relying on the estimate.

## Operating systems

Linux is the supported runtime for untrusted media. The FFmpeg child receives an
address-space limit and `no_new_privs` before it executes.

macOS is suitable for development and tests, but macOS does not enforce the decoder
address-space limit. The deadline and output ceilings still apply. `matrix-media` is
Unix-oriented; Windows is not a supported build target.

The published CI image is currently built and tested on Linux/amd64. Other container
architectures are not part of the release contract until they receive the same image
smoke test.

## MCP clients

Clients must support MCP `2026-07-28` over Streamable HTTP and connect to `/mcp`. The
server is stateless at the transport layer; asset and playback handles are ordinary tool
arguments and become invalid after a server restart.
