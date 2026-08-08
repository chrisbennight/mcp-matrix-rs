# DECISIONS

Design constraints and rationale that govern the current implementation of
`mcp-matrix-rs`.

## Render server-side rather than through WLED's own text and effects

WLED can scroll text and run effects natively, and the M-1 ships with both. This server
does neither, and rasterizes frames itself.

Native rendering caps scrolling text at 64 characters on stock WLED and 32 on WLED-MM,
which is unusable for a general display surface. Effect IDs shift between firmware
versions, so an unattended caller that learned an ID silently gets different behaviour
after a reflash. And segment reconfiguration over HTTP on every update is a poor fit for
anything with layout.

Owning the framebuffer removes all three, plus the 4096-pixel canvas becomes a drawing
surface rather than a set of segments. The cost is owning font rendering and the render
loop. Native effects survive as ambient configuration — what plays when nothing is
scheduled.

## DDP rather than WLED's own UDP realtime protocol

Both reach the panel. DNRGB caps at 489 pixels per packet, so a 4096-pixel frame is nine
packets either way, and neither compresses.

DDP wins on two counts: its header carries an offset and length, which leaves
dirty-region transmission available without a protocol change, and it is the transport
with the widest existing tooling. E1.31 and Art-Net are worse for this — DMX
compatibility caps them near 40 fps and adds overhead this application has no use for.

## Fixed-rate frame sequences rather than per-frame timing

GIFs carry per-frame delays that vary within a single file. Video has its own rate.
Carrying that variability into the internal representation would put it in the playout
loop permanently.

Resampling to a fixed rate at ingest confines all timing complexity to one place, and
leaves playout as a branchless fixed tick reading `frames[i]`. Every source type is
byte-identical from the pump's perspective.

## Framerate is measured, not assumed

Arithmetic says a 4096-pixel frame is 12,288 bytes, nine DDP packets, and roughly
370 KB/s at 30 fps. That arithmetic says nothing about what an ESP32 sustains while
simultaneously receiving those packets and driving HUB75 refresh.

WLED reports `leds.fps`. That is the number that decides the pump's rate, whether
dirty-region transmission is worth building, and whether the design needs to change.
`matrix-frame`'s `MAX_RATE_FPS` is an input-validation ceiling that rejects an absurd
frame budget, not a target the pump aims at.

## `Frame::set` clips silently; `Frame::get` returns `Option`

The asymmetry is deliberate. Drawing routines clip against the canvas edge constantly —
a scrolling glyph is partly off-canvas for most of its life — so a fallible setter would
push a bounds check into every caller for no safety gain, since `Canvas::offset` already
performs it. Reading is different: a caller asking for a pixel outside the canvas has a
coordinate bug, and `None` says so.

The cost is that a caller writing to a bad coordinate sees nothing rather than an error.
That cost is accepted.

## Scaling happens in sRGB space, not linear light

Downscaling averages neighbouring pixels, and averaging gamma-encoded values is not the
same operation as averaging light. The correct result comes from converting to linear,
scaling, and converting back; doing it in the encoded space makes a reduced image darker
than the scene it came from, and the error grows with the reduction ratio. At 30x it is
visible.

The filtergraph scales in the input transfer space because `zscale` is available only in
FFmpeg builds compiled against zimg. Requiring it would make decoding fail on otherwise
supported FFmpeg installations.

Normalized output is therefore dimmer than a linear-light downscale would produce.
Linear-light scaling remains possible with a `zscale` pair around the scale filter once
the supported FFmpeg baseline guarantees zimg.

## Text renders from const bitmap font data

`matrix-text` embeds `font8x8` (crates.io, v0.3.1, MIT-licensed crate wrapping
public-domain 8x8 bitmap font data, no dependencies). Glyphs are const arrays: no
parsing, no allocation, no rasterizer attack surface, and a vector font would be
wasted on a 64-pixel panel. Characters outside coverage render as `?` rather than
refusing the message. Text obeys the same frame budget the media path decodes under.
