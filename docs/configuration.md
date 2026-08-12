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
| `MATRIX_FFMPEG_BIN` | no | `ffmpeg` | FFmpeg executable path |
| `MATRIX_FFPROBE_BIN` | no | `ffprobe` | ffprobe executable path |
| `MATRIX_STDIO` | no | off | Serve MCP over stdio to the spawning client instead of listening on HTTP |
| `MATRIX_FILE_PUBLIC_URL` | no | unset | Public HTTPS origin this server is reached at. Setting it enables the transfer plane; unset leaves the server inline-only |
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

## The transfer plane

Unset `MATRIX_FILE_PUBLIC_URL` is the default and means inline-only: `matrix_submit_asset`
takes a `data:` URI up to 16 KiB and nothing else, `files/authorizeUpload` answers
method-not-found, and the published tool schema says nothing about files. That
method-not-found is the contract — a file-aware intermediary reads it as "this upstream
has no native file transfer" and stops asking.

Setting it turns on the receiving side of the draft file-transfer contract, which is how
media past the inline cap reaches the panel. Three constraints come with it:

- **It must be an `https` origin, scheme and authority only.** A path, query, userinfo,
  or plain `http` is refused at startup. The value is what a trusted intermediary dials,
  and it will refuse a descriptor that is not HTTPS with a certificate valid against the
  system roots. This server does not terminate TLS; the operator's boundary does.
- **It should be the origin `/mcp` is already served on.** An intermediary reuses the
  addresses it pinned for the MCP connection only when the transfer descriptor names that
  same host; a descriptor naming a different host must resolve to a publicly routable
  address. Same origin therefore keeps private addressing working and needs no new
  `MATRIX_ALLOWED_HOSTS` entry. A different port on the same host also works.
- **It requires the HTTP transport.** Combining it with `MATRIX_STDIO` is refused at
  startup, because there is no listener to receive a transfer on.

The staging directory must be writable by the server's user. The container image declares
no volume for it, so mount a `tmpfs` or a volume when the plane is enabled — see
[deployment](deployment.md#file-staging).

`MATRIX_FILE_MAX_STAGED` bounds outstanding authorizations and staged transfers together,
and `MATRIX_FILE_TTL_SECS` bounds how long either survives unused. Both exist so a caller
that authorizes transfers it never completes cannot accumulate disk; a sweeper collects
what expires. Neither is a rate limit, and this server still has no client
authentication — the operator owns that boundary.
