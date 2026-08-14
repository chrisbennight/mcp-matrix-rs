#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 IMAGE" >&2
  exit 2
fi

image=$1
smoke_container="matrix-smoke-$$"
smoke_dir=$(mktemp -d)

cleanup() {
  task_exit=$?
  docker logs "$smoke_container" 2>&1 || true
  docker rm -f "$smoke_container" >/dev/null 2>&1 || true
  rm -r "$smoke_dir"
  return "$task_exit"
}
trap cleanup EXIT

docker run --rm --entrypoint /usr/bin/ffmpeg "$image" -version >/dev/null
docker run --rm --entrypoint /usr/bin/ffprobe "$image" -version >/dev/null

docker run -d --name "$smoke_container" \
  -e MATRIX_WLED_URL=http://127.0.0.1:1 \
  -e MATRIX_DDP_ADDR=127.0.0.1:4048 \
  "$image" >/dev/null

ready=
for _ in $(seq 1 30); do
  if docker exec "$smoke_container" \
    /usr/local/bin/matrix-server --healthcheck >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
test -n "$ready"

docker run --rm -i --network "container:${smoke_container}" \
  python:3.14-slim@sha256:b877e50bd90de10af8d82c57a022fc2e0dc731c5320d762a27986facfc3355c1 \
  python3 - <<'PY' > "$smoke_dir/tools.json"
import json, urllib.request
body = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/list",
    "params": {"_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    }},
}
request = urllib.request.Request(
    "http://127.0.0.1:8080/mcp",
    data=json.dumps(body).encode(),
    headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": "tools/list",
        "Mcp-Name": "tools/list",
    },
)
with urllib.request.urlopen(request, timeout=10) as response:
    print(response.read().decode())
PY

python3 - "$smoke_dir/tools.json" <<'PY' > "$smoke_dir/actual-tools.txt"
import json, sys
with open(sys.argv[1]) as fh:
    result = json.load(fh)["result"]
assert result.get("resultType") == "complete", result.get("resultType")
assert result.get("ttlMs", 0) > 0, result.get("ttlMs")
assert result.get("cacheScope") == "private", result.get("cacheScope")
for tool in result["tools"]:
    print(tool["name"])
PY

diff -u smoke/expected-tools.txt "$smoke_dir/actual-tools.txt"

# Decode a source larger than the canvas, which is the case the panel exists for: every
# GIF and video has to be downscaled to reach it, and the scale filter is what adds the
# FFmpeg worker threads whose arenas dominate the decoder's address-space reservation.
#
# This asserts the image can do that decode on whatever host is running the smoke. It is
# not a guard on the size of the ceiling: how much address space FFmpeg needs varies with
# the host's architecture and CPU count, so a ceiling that fails on a wide amd64 machine
# can pass here. The ceiling being wired to the operator's setting at all is what the
# refusal below pins.
docker run --rm -i --network "container:${smoke_container}" \
  python:3.14-slim@sha256:b877e50bd90de10af8d82c57a022fc2e0dc731c5320d762a27986facfc3355c1 \
  python3 - <<'PY'
import base64, json, struct, urllib.request, zlib

width = height = 256
# Banded rather than a per-pixel gradient: the source only has to be larger than the
# canvas to force the scale, and it has to stay under the 16 KiB inline cap once
# base64-encoded, which a noisy image would not.
raw = b"".join(
    b"\x00" + bytes(((y * 3) % 256, (y * 7) % 256, 140)) * width for y in range(height)
)


def chunk(kind, payload):
    body = kind + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))


png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(raw, 9))
    + chunk(b"IEND", b"")
)
source = "data:image/png;base64," + base64.b64encode(png).decode()
assert len(source) < 16 * 1024, f"inline source must fit the inline cap, got {len(source)}"

body = {
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/call",
    "params": {
        "name": "matrix_submit_asset",
        "arguments": {"source": source},
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        },
    },
}
request = urllib.request.Request(
    "http://127.0.0.1:8080/mcp",
    data=json.dumps(body).encode(),
    headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": "tools/call",
        "Mcp-Name": "matrix_submit_asset",
    },
)
with urllib.request.urlopen(request, timeout=30) as response:
    payload = json.loads(response.read().decode())

assert "error" not in payload, payload["error"]
result = payload["result"]
assert not result.get("isError"), result
report = json.loads(result["content"][0]["text"])
assert report["frames"] >= 1, report
print(f"scaled decode ok: {report}")
PY

# The same decode under a ceiling too small for FFmpeg to start, which is the failure an
# operator on a wide host hits and the reason the setting exists. It has to be refused
# rather than decoded: if the setting never reached the decoder subprocess, this would
# succeed exactly like the call above, and the operator's only remedy would be silently
# inert. The refusal must be the decoder's, not a rejection before the subprocess runs.
starved_container="matrix-smoke-starved-$$"
docker run -d --name "$starved_container" \
  -e MATRIX_WLED_URL=http://127.0.0.1:1 \
  -e MATRIX_DDP_ADDR=127.0.0.1:4048 \
  -e MATRIX_DECODER_ADDRESS_SPACE_MB=16 \
  "$image" >/dev/null
trap 'docker rm -f "$starved_container" >/dev/null 2>&1 || true; cleanup' EXIT

starved_ready=
for _ in $(seq 1 30); do
  if docker exec "$starved_container" \
    /usr/local/bin/matrix-server --healthcheck >/dev/null 2>&1; then
    starved_ready=1
    break
  fi
  sleep 1
done
test -n "$starved_ready"

docker run --rm -i --network "container:${starved_container}" \
  python:3.14-slim@sha256:b877e50bd90de10af8d82c57a022fc2e0dc731c5320d762a27986facfc3355c1 \
  python3 - <<'PY'
import base64, json, struct, urllib.request, zlib

width = height = 256
raw = b"".join(
    b"\x00" + bytes(((y * 3) % 256, (y * 7) % 256, 140)) * width for y in range(height)
)


def chunk(kind, payload):
    body = kind + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))


png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(raw, 9))
    + chunk(b"IEND", b"")
)

body = {
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
        "name": "matrix_submit_asset",
        "arguments": {
            "source": "data:image/png;base64," + base64.b64encode(png).decode()
        },
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        },
    },
}
request = urllib.request.Request(
    "http://127.0.0.1:8080/mcp",
    data=json.dumps(body).encode(),
    headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": "tools/call",
        "Mcp-Name": "matrix_submit_asset",
    },
)
with urllib.request.urlopen(request, timeout=30) as response:
    payload = json.loads(response.read().decode())

detail = json.dumps(payload)
assert "media_decoder_failed" in detail, f"starved decoder must refuse, got {detail}"
print("starved decoder refused as configured")
PY
