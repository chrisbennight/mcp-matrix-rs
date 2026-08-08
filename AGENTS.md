# AGENTS.md

Guidance for working in `mcp-matrix-rs`.

## Overview

`mcp-matrix-rs` is a realtime render server for a WLED LED matrix. It accepts text
and media through MCP, normalizes content to fixed-rate RGB frames, and drives the
panel over DDP.

- Repository: `https://github.com/chrisbennight/mcp-matrix-rs`
- Verified hardware: Apollo Automation M-1, 64x64 RGB HUB75, WLED firmware
- Protocol: MCP `2026-07-28` over Streamable HTTP at `/mcp`
- Trust model: the application has no built-in authentication; operators define the
  trusted boundary

## Architecture and roadmap

`PLAN.md` records the architecture and roadmap. `DECISIONS.md` records design choices
and their reasoning. Read both before changing an architectural boundary.

## Architecture

- `crates/matrix-frame` owns canvas geometry and the fixed-rate frame representation.
  It has no I/O, async runtime, or device knowledge.
- `crates/matrix-device` owns the WLED JSON client and DDP transport.
- `crates/matrix-media` owns untrusted media decode and normalization. Decoding runs
  out of process under a deadline and resource ceilings.
- `crates/matrix-playout` owns rate adaptation, power clamping, and paced frame sends.
  `Playout` is the only frame-send path.
- `crates/matrix-text` rasterizes text into the same frame representation.
- `crates/matrix-server` owns the binary, MCP transport, tool dispatch, and shared
  engine state.

`smoke/expected-tools.txt` is the release catalog. The image smoke compares the exact
advertised tool names against it.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker build -t mcp-matrix-rs:local .
```

CI targets Linux/amd64. Unit and integration tests must use loopback fakes and never
contact a real panel or another user-owned system.

## Code style

- Rust 2024 edition; `cargo fmt` is the formatter and Clippy warnings are errors.
- Use `thiserror` for library errors with a stable `.code()` and `anyhow` only at the
  binary boundary.
- Use `tracing` for logs. Do not commit debug prints or expose submitted content,
  credentials, or full environment/configuration objects in logs.
- Prefer precise types and strong invariants. Comments should state a constraint the
  code cannot express.
- Untrusted input never reaches a shell and never selects a fetch destination. Pass
  subprocess arguments as argv entries.
- Preserve the one send path through `Playout`; bypassing it also bypasses rate and
  power enforcement.

## Repository boundary

This repository contains portable application source, tests, a generic container,
public CI, and example configuration. It must not contain a particular deployment's
hostnames, IP addresses, secret-provider identifiers, registry credentials, reverse
proxy rules, orchestrator configuration, monitoring wiring, or hardware smoke targets.

Examples use documentation-only addresses and safe loopback exposure. Real deployment
configuration belongs to the operator's private infrastructure repository.

## Device and security boundaries

WLED JSON over HTTP carries configuration and state. DDP over UDP port 4048 carries
frames. Do not send frames through the JSON plane or configuration through DDP.

The server has no built-in client authentication and all callers share engine state.
The Host allowlist is a DNS-rebinding defense, not authorization. Keep the endpoint on
loopback or a trusted private network, or place it behind an authenticated boundary.

Keep WLED's bounded realtime timeout enabled so a server failure returns the panel to
its ambient state. Treat the M-1 power model as hardware-specific; do not claim another
panel is supported without verified electrical and protocol evidence.

## Git workflow

- Start work in a dedicated worktree created from freshly fetched `origin/main`.
- Never push directly to `main`.
- Stage specific paths rather than all working-tree changes.
- Ask before committing, pushing, or opening a pull request unless the user explicitly
  authorized those actions.
- Run the relevant verification commands and read their explicit successful exit status
  before each commit or push.
- Keep pull-request descriptions factual: design goal, externally observable outcomes,
  implementation narrative, verification, risks, and non-goals.

## Boundaries

Do not commit secrets, production deployment files, or test code that makes live calls
to a panel or shared network service. Do not dereference a caller-selected URL. A future
artifact-transfer integration must resolve and authorize content before this server
receives it, then preserve the existing decode limits.
