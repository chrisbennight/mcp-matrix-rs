# Contributing

Changes should preserve the separation between frame representation, device I/O, media
ingest, playout, text rendering, and the MCP server. Read `PLAN.md` and `DECISIONS.md`
before changing an architectural boundary.

Use a feature branch or worktree based on the latest `main`. Before opening a pull
request, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Changes to the container or MCP tool catalog should also build the image and exercise
the smoke flow in `.github/workflows/image.yml`.

Tests must not contact a real WLED device or another user-owned system. Use loopback
servers, UDP listeners, controlled subprocesses, and temporary directories. A behavior
change should include regression evidence at the layer that owns the contract.

Submitted media is untrusted. Never pass caller input through a shell, fetch a
caller-selected destination, remove the decoder deadline or resource ceilings, or add a
frame send path that bypasses playout and its power clamp.
