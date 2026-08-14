# Deployment and security

`matrix-server` controls a physical device and accepts media for FFmpeg to decode. Treat
the MCP endpoint as a trusted administrative surface.

## Trust boundary

The application does not authenticate clients and does not implement tenant isolation.
All clients share the resident asset store and current playback; any client that can
call the tools can replace or stop what another client started, change brightness, or
turn the panel on and off. The Host-header allowlist prevents DNS rebinding but does not
authorize a caller.

Use one of these deployment shapes:

- Run the standalone binary with `--stdio` as a child process the MCP client spawns.
  The OS process boundary is the trust boundary, and there is exactly one client per
  process. Each stdio process assumes it is the panel's only driver: run one per panel
  and do not combine it with a concurrently serving HTTP instance, or clients will
  contend for the frame stream without seeing each other's state.
- Bind the standalone binary to its default loopback address for a client on the same
  host.
- Publish the example container port on loopback and connect through an authenticated
  local gateway or reverse proxy.
- Place the service on a trusted private network with firewall policy restricting both
  the MCP listener and the panel-facing network.

Never publish port 8080 on an internet-facing interface without an authenticated
boundary in front of it. TLS belongs at that boundary; the application serves plaintext
HTTP.

## Container posture

The supplied image runs as an unprivileged user and uses `tini` to reap decoder
subprocesses. The example Compose file also drops Linux capabilities, prevents privilege
gain, uses a read-only root filesystem, and provides only a temporary `/tmp`.

Set deployment-specific memory, CPU, and process limits based on the configured canvas.
The default decode path permits up to 24 MiB of stored frames while bounding the
temporary raw-plus-assembled peak at 48 MiB. The in-memory store retains at most eight
assets, and each decoder receives a 2 GiB virtual address-space ceiling on Linux. Do not
choose a container memory limit without exercising concurrent decode and playback at the
configured canvas size.

That address-space ceiling is virtual, not resident, and it is the one limit here that
depends on the host rather than on the canvas: FFmpeg sizes its worker threads from the
CPUs it can see, and each thread costs a 64 MiB glibc arena reservation. The default
suits an eight-core host. On a machine with substantially more cores, decodes that need
a downscale — which is every GIF and video — can fail while panel-sized stills still
succeed. `MATRIX_DECODER_ADDRESS_SPACE_MB` in [configuration](configuration.md#decoder-address-space)
covers the symptom and how to size it.

The decoder receives no inherited environment and cannot fetch a caller-selected URL.
The MCP tool surface resolves a base64 `data:` URI, and — when the transfer plane is
enabled — a reference this server itself minted for bytes already in its own staging
directory. Neither is a fetch: there is no code path that dereferences a destination a
caller named.

## File staging

The transfer plane is off unless `MATRIX_FILE_PUBLIC_URL` is set. When it is enabled,
bytes are pushed to this server rather than pulled by it: a trusted intermediary asks for
an upload authorization, this server returns a single-use descriptor pointing at its own
`PUT /files/upload/{id}` route, and the intermediary streams the media there. Only then
does an ordinary `matrix_submit_asset` call name the staged source.

That route rides the same listener as `/mcp`, so it sits behind whatever authenticated
boundary and TLS termination already front the service. `MATRIX_FILE_PUBLIC_URL` must be
the origin clients reach that boundary at, and should be the same host `/mcp` is served
on — an intermediary reuses its pinned MCP addresses only for a descriptor naming that
host, and otherwise requires a publicly routable one.

Where there is no boundary in that path — this server running as a private sidecar an
intermediary reaches directly over a container network, with no proxy and no TLS — the
origin may be `http`. That is sound only because the intermediary already sends this
server its tool arguments and file references in cleartext across the same segment, so
the byte stream gives up nothing the control plane has not. It refuses a plaintext
descriptor for any other kind of destination. This server cannot verify which case it is
in, so configuring an `http` origin is the operator asserting it.

The staging directory must be **dedicated to one server instance**, and writable — which
the read-only root filesystem in the example Compose file does not provide by default.
Mount a `tmpfs` at a path of its own, sized for `MATRIX_FILE_MAX_STAGED` times the 64 MiB
source ceiling.

Do not point it at a shared directory such as the container's `/tmp`. Two instances
sharing one would discard each other's transfers in progress, and nothing coordinates
them.

The server reduces the damage of getting this wrong without being able to eliminate it.
It sets permissions only on a directory it created itself, so it will not re-permission
one you provisioned. At startup it removes only files whose names have the shape it
mints — 43 base64url characters, optionally with a `.part` suffix — so ordinary files
sharing the directory survive. That is a heuristic on the name and not proof of
provenance: another process writing files of the same shape into the same directory
would still lose them. A dedicated mount is the supported arrangement and the only one
whose capacity and contents you can reason about.

Startup discards what a previous run left behind. A process killed without unwinding runs
no cleanup, so its partial and unconsumed files would otherwise survive with nothing left
that knows about them: invisible to the sweeper, uncounted against the ceiling, and never
removed.

Newly created staging directories are restricted to the server's user, as are the files
in them.

Transfer authority is single-use, expiring, and minted from the platform CSPRNG. It
travels in a request header rather than the descriptor URL so it stays out of proxy
access logs. It is transfer authority, not identity: this server still has no client
authentication, and the operator still owns that boundary.

## Network requirements

The server initiates HTTP requests to WLED and sends UDP DDP packets to the configured
panel. Restrict its egress to that device where the deployment platform supports it.
Keep WLED's realtime timeout enabled so an interrupted server returns the panel to its
ambient behavior instead of freezing the last frame.

Do not place credentials in `MATRIX_WLED_URL`. Target URLs and device failures may be
present in operational logs and errors.

## Health and operation

`GET /healthz` and `matrix-server --healthcheck` prove only that the HTTP process is
serving. They do not prove the panel is reachable. Use `matrix_describe_device` as the
device readiness check and verify the reported dimensions, frame rate, and power ceiling
before first playback.

A stdio instance has no liveness endpoint, and `--stdio` combined with `--healthcheck`
is rejected at startup; the spawning client observes process health directly, and
`matrix_describe_device` remains the device readiness check.

The server keeps assets only in memory. Restarting it clears assets and playback
handles; WLED's realtime timeout then restores ambient behavior.
