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
assets, and each decoder receives a 1 GiB virtual address-space ceiling on Linux. Do not
choose a container memory limit without exercising concurrent decode and playback at the
configured canvas size.

The decoder receives no inherited environment and cannot fetch a caller-selected URL.
The MCP tool surface resolves only base64 `data:` URIs.

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

The server keeps assets only in memory. Restarting it clears assets and playback
handles; WLED's realtime timeout then restores ambient behavior.
