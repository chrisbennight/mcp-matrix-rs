# Configuration

`matrix-server` accepts each setting as a command-line option and as the environment
variable listed below. Run `matrix-server --help` for the corresponding option names.

| Environment variable | Required | Default | Meaning |
| --- | --- | --- | --- |
| `MATRIX_WLED_URL` | yes | none | WLED device origin, such as `http://192.0.2.10` |
| `MATRIX_DDP_ADDR` | yes | none | Numeric panel IP and DDP port, normally `192.0.2.10:4048` |
| `MATRIX_HTTP_ADDR` | no | `127.0.0.1:8080` | Streamable HTTP listener; the container overrides this to `0.0.0.0:8080` |
| `MATRIX_ALLOWED_HOSTS` | no | `localhost,127.0.0.1,::1` | Complete comma-separated Host-header allowlist for `/mcp` |
| `MATRIX_WIDTH` | no | `64` | Render canvas width in pixels |
| `MATRIX_HEIGHT` | no | `64` | Render canvas height in pixels |
| `MATRIX_TARGET_FPS` | no | `25` | Requested whole-frame rate from 1 through 240 |
| `MATRIX_DEVICE_TIMEOUT_MS` | no | `3000` | Timeout for WLED JSON API calls |
| `MATRIX_DECODER_ADDRESS_SPACE_MB` | no | `2048` | Address-space ceiling imposed on each decoder subprocess. Raise it on a host with many CPUs; see below |
| `MATRIX_FFMPEG_BIN` | no | `ffmpeg` | FFmpeg executable path |
| `MATRIX_FFPROBE_BIN` | no | `ffprobe` | ffprobe executable path |
| `MATRIX_STDIO` | no | off | Serve MCP over stdio to the spawning client instead of listening on HTTP |
| `MATRIX_FILE_PUBLIC_URL` | no | unset | Public origin this server is reached at. Setting it enables the transfer plane; unset leaves the server inline-only |
| `MATRIX_FILE_STAGING_DIR` | no | `/tmp/matrix-staging` | Directory transfers are staged in. Created if absent and restricted to the server's own user |
| `MATRIX_FILE_TTL_SECS` | no | `300` | How long an unused authorization or an unconsumed staged transfer survives |
| `MATRIX_FILE_MAX_STAGED` | no | `4` | Ceiling on authorizations and staged transfers outstanding at once |

The width and height must describe the WLED matrix layout. The server reports the
dimensions returned by WLED but does not currently reconcile a mismatch automatically.
Confirm the result with `matrix_describe_device` before displaying content.

`MATRIX_DDP_ADDR` is parsed as a socket address and therefore requires an IP literal,
not an mDNS or DNS hostname. The HTTP URL may use a hostname.

`MATRIX_ALLOWED_HOSTS` is a DNS-rebinding defense, not authentication. Include every
authority through which an MCP client reaches the service, without a URL scheme. The
`/healthz` liveness endpoint is intentionally outside this check.

With `MATRIX_STDIO` (or `--stdio`) set, the server speaks MCP over its own stdin and
stdout for the one client that spawned it. `MATRIX_HTTP_ADDR` and
`MATRIX_ALLOWED_HOSTS` are ignored, and combining the flag with `--healthcheck` is
rejected: there is no HTTP endpoint to probe, and silently probing one anyway would
report the wrong process's health. All logging goes to stderr in both modes, so
stdout stays valid JSON-RPC.

The container pins FFmpeg and ffprobe to `/usr/bin`. Override their paths only when
running the standalone binary with a deliberately selected installation.

## Decoder address space

On Linux, each decode runs as a subprocess under an address-space ceiling, so a source
that declares an enormous frame is refused by the kernel instead of exhausting the
container. `MATRIX_DECODER_ADDRESS_SPACE_MB` sets that ceiling.

**The ceiling is enforced only on Linux.** The setting is accepted everywhere, but no
kernel bound is applied on other targets — macOS rejects `setrlimit(RLIMIT_AS)` outright,
so applying it would fail the spawn rather than bound it. The decode deadline and the
output ceilings still apply there. See [compatibility](compatibility.md#operating-systems);
Linux is the supported runtime for untrusted media, and nothing below changes that.

**The default is sized for an eight-core host, and the requirement follows the host, not
the media.** FFmpeg sizes its worker threads from the CPU count it can see, and glibc
reserves a 64 MiB malloc arena per thread, so nearly all of the reservation is a startup
cost. Across a 3600-fold range of source pixels the requirement moves by single-digit
percent; going from one visible core to eight nearly doubles it. A machine with many
more cores than eight can therefore need a higher ceiling for exactly the same media.

The symptom is `media_decoder_failed` with FFmpeg reporting `Resource temporarily
unavailable`, `ff_frame_thread_encoder_init failed`, or a filter that could not be
configured. **Scaled sources fail first, and a native-resolution still may keep working**
— the scale filter is what adds the threads — so the failure looks like it depends on the
media when it depends on the host. If large media fails while panel-sized stills succeed,
raise this before looking anywhere else.

To find a working value on a specific host, run the decoder's own filtergraph under a
candidate ceiling and raise it until it succeeds:

```bash
( ulimit -v $((2048 * 1024)); ffmpeg -nostdin -i source.png -vf "fps=25,scale=64:64:flags=area,format=rgb24" -f rawvideo -y /dev/null )
```

Raising this weakens the bound it provides, so raise it to what the host needs rather
than to an arbitrarily large number. Limiting the CPUs the container can see is the other
lever, and it lowers the requirement, but it also costs decode throughput.

## The transfer plane

Unset `MATRIX_FILE_PUBLIC_URL` is the default and means inline-only: `matrix_submit_asset`
takes a `data:` URI up to 16 KiB and nothing else, `files/authorizeUpload` answers
method-not-found, and the published tool schema says nothing about files. That
method-not-found is the contract — a file-aware intermediary reads it as "this upstream
has no native file transfer" and stops asking.

Setting it turns on the receiving side of the draft file-transfer contract, which is how
media past the inline cap reaches the panel. Four constraints come with it:

- **It must be a bare origin, scheme and authority only.** A path, query, or userinfo is
  refused at startup. `https` is the normal choice, and this server does not terminate
  TLS — the operator's boundary does.
- **`http` is accepted, and is only sound on a private segment.** An intermediary
  admits a plaintext transfer descriptor only where it already reaches this
  server's `/mcp` in cleartext over a pinned private address; it refuses one from
  anywhere else. Whether that describes your deployment is a fact about its topology
  that this server cannot observe, so configuring `http` is your assertion that it does.
- **It should be the origin `/mcp` is already served on.** An intermediary reuses the
  addresses it pinned for the MCP connection only when the transfer descriptor names that
  same host; a descriptor naming a different host must resolve to a publicly routable
  address. Same origin therefore keeps private addressing working and needs no new
  `MATRIX_ALLOWED_HOSTS` entry. A different port on the same host also works.
- **It requires the HTTP transport.** Combining it with `MATRIX_STDIO` is refused at
  startup, because there is no listener to receive a transfer on.

The staging directory must be writable by the server's user and dedicated to one server
instance — not a shared path such as `/tmp`. The container image declares no volume for
it, so mount a `tmpfs` or a volume of its own when the plane is enabled; see
[deployment](deployment.md#file-staging).

`MATRIX_FILE_MAX_STAGED` bounds outstanding authorizations and staged transfers together,
and `MATRIX_FILE_TTL_SECS` bounds how long either survives unused. Both exist so a caller
that authorizes transfers it never completes cannot accumulate disk; a sweeper collects
what expires. Neither is a rate limit, and this server still has no client
authentication — the operator owns that boundary.
