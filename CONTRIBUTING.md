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
the smoke flow in `.github/workflows/test.yml`.

Tests must not contact a real WLED device or another user-owned system. Use loopback
servers, UDP listeners, controlled subprocesses, and temporary directories. A behavior
change should include regression evidence at the layer that owns the contract.

Submitted media is untrusted. Never pass caller input through a shell, fetch a
caller-selected destination, remove the decoder deadline or resource ceilings, or add a
frame send path that bypasses playout and its power clamp.

CI selects checks from the changed inputs. Rust tests do not build an image;
runtime and image packaging changes build and smoke the image after source
checks pass. Markdown skips those costly steps. Manual runs select all checks,
and unavailable Git history fails selection. The existing `rust` and `build`
jobs run together; the duplicate main-push validation is removed. Version-tag
releases run format, Clippy and tests before building and smoking the image,
then publish that same image.
