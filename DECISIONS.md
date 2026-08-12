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

## stdio as a flag on the one binary rather than a second binary

Some MCP clients cannot reach a remote server at all; they only spawn a local process
and speak JSON-RPC over its stdin and stdout. `matrix-server --stdio` serves them with
the same `MatrixHandler` the Streamable HTTP transport uses, so the advertised tool
catalog cannot drift between transports and `smoke/expected-tools.txt` stays the single
release catalog.

A separate stdio binary would double the release surface for no behavioural gain, and
serving both transports from one process would blur process ownership: stdio's contract
is that the spawning client owns the lifetime, ending the session by closing stdin.
Process exit stops the device poller, and WLED's realtime timeout returns the panel to
ambient — the same failure posture as the HTTP server dying.

The trade is concurrency. The HTTP server multiplexes callers over one engine; each
stdio process is a full engine that believes it exclusively owns the panel. That fits
the intended audience — one desktop client per panel — and the deployment guide says
so rather than the server attempting cross-process coordination.

## Repeating scrollers tile the package rather than extending it

A layout scroller crosses its rectangle once and parks, so a composition mixing a
long chyron with short vertical tickers leaves the short regions blank for most of
the package. The fix keeps the bounded-package model: a `repeat` scroller runs
`floor(package / crossing)` evenly spaced crossings — always a whole number — inside
the unchanged package length, and a `phase` offsets where in its cycle the region
starts.

Two alternatives lost. Giving each region its own free-running period needs a
package as long as the least common multiple of every period to loop seamlessly,
which explodes past the frame budget for almost any real mix of speeds. Rendering
text procedurally at playout time would avoid pre-rendering entirely but breaks two
standing invariants: one internal representation for all content, and ingest never
touching playout.

The trade is that cycles are quantized to the package: a region's true period is
`package / k`, not exactly `crossing`, and every region still shares one global
restart every package. Whole-cycle tiling keeps that restart invisible: the wrap
continues each region's motion by exactly one ordinary step, never a jump. For an
unphased region the wrap falls in its idle gap; a phase can place it mid-crossing,
where the glyphs are visible but still advance as if no seam existed.

## Media is pushed to this server, never fetched by it

The obvious way to accept media past the inline cap is to take a URL and fetch it. That
is the confused-deputy pattern this server refuses: the caller chose the destination, the
caller can be prompt-injected, and dereferencing it would make this server their user
agent. The blast radius is small — the worst case is an attacker's picture on a wall —
but it is the wrong design centre.

The next-most-obvious alternative is to fetch only from an operator-configured transfer
endpoint. That is better, and still wrong for this server: it means holding someone
else's credential, having a destination that can be misconfigured, and owning an outbound
HTTP client whose failure modes are ours.

What actually ships inverts the direction. A trusted intermediary calls
`files/authorizeUpload` on this server; this server mints a single-use ticket and returns
a descriptor pointing at its own ingest route; the intermediary streams the bytes there.
There is no fetch anywhere in the server, no credential held for anyone else's endpoint,
and no destination to get wrong. It also needs no per-deployment transfer configuration
beyond this server's own public origin, and it is what the draft file-transfer contract
already expects an upstream to do.

The cost is that the plane only works where this server is reachable over HTTPS at a
hostname an intermediary can dial — which means it is unavailable under `--stdio`, and
that combination is refused at startup rather than silently minting descriptors nothing
can reach.

## Nothing is advertised when the transfer plane is off

An unconfigured server answers `files/authorizeUpload` with method-not-found and
publishes a tool schema with no file annotation. Both are deliberate and are the same
decision: a file-aware intermediary reads `-32601` as "this upstream has no native file
transfer", so the absence *is* the advertisement, and a schema that annotated a file input
the server would then refuse would be worse than saying nothing.

That is also why the annotation is attached when the tool list is served rather than
derived from the parameter type. The published contract has to follow the deployment, and
an inline-only deployment must publish exactly what it published before this existed.

## A staged reference is unguessable because nothing else authenticates it

Ordinary asset and playback handles are a per-process token plus a counter, and are
documented as an identifier namespace rather than a security boundary. A transfer
credential cannot be that: authorization and consumption are separate requests, this
server has no client authentication, and the ticket is the only thing linking them.

Ticket identifiers and transfer credentials therefore come from the platform CSPRNG, the
credential is stored hashed and compared in constant time, and it travels in a request
header rather than the descriptor URL so it stays out of proxy access logs. An unknown
ticket and a wrong credential return the same refusal, because distinguishing them would
confirm which identifiers exist.

None of that is a substitute for the operator's boundary. It bounds what a leaked
descriptor is worth — one transfer, of one declared size and digest, for a few minutes.
