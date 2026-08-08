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
