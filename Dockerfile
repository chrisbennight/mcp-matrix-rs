# syntax=docker/dockerfile:1@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89

ARG SOURCE_REVISION=unknown

# Builder: compile matrix-server against the pinned toolchain. The whole workspace is
# copied because cargo must parse every member to build one crate; `.dockerignore`
# keeps the context lean.
FROM rust:1.96-trixie@sha256:1f0dbad1df66647807e6952d1db85d0b2bda7606cb2139d82517e4f009967376 AS build
WORKDIR /app
COPY . .
RUN cargo build --release --locked --bin matrix-server
RUN strip target/release/matrix-server || true

# Runtime: debian-slim rather than distroless, because the media path shells out to
# FFmpeg and ffprobe and those arrive through apt. The decoder is a subprocess by
# design — it parses caller-supplied media, and a separate process can be given a
# deadline, an address-space limit, and a kill.
FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd AS runtime
RUN apt-get update \
 && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      ffmpeg \
      tini \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin app

COPY --from=build /app/target/release/matrix-server /usr/local/bin/matrix-server

ARG SOURCE_REVISION
LABEL org.opencontainers.image.source="https://github.com/chrisbennight/mcp-matrix-rs" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.matrix.role="server"

USER 10001

# The panel addresses have no defaults, so a misconfigured container fails at startup
# rather than sending frames somewhere arbitrary.
# The container listens on all interfaces so a deliberately published port works; the
# standalone binary defaults to loopback. The decoder paths are pinned so the media path
# cannot pick up a different binary from PATH.
ENV MATRIX_HTTP_ADDR=0.0.0.0:8080 \
    MATRIX_WIDTH=64 \
    MATRIX_HEIGHT=64 \
    MATRIX_TARGET_FPS=25 \
    MATRIX_FFMPEG_BIN=/usr/bin/ffmpeg \
    MATRIX_FFPROBE_BIN=/usr/bin/ffprobe
EXPOSE 8080

# Exec form: the binary's own --healthcheck hits loopback /healthz and exits 0 or 1,
# so the runtime image needs neither a shell nor curl on the health path.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/matrix-server", "--healthcheck"]

# tini reaps the FFmpeg children the media path spawns; without it a killed decoder
# lingers as a zombie under PID 1.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/matrix-server"]
