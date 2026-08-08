# Security policy

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for issues that could expose a deployment,
escape the media-decoder boundary, bypass resource limits, or permit unauthorized device
control. If private reporting is unavailable, open a public issue requesting a private
contact channel without including vulnerability details. Do not include credential
values, private deployment addresses, or weaponized media in a public issue.

Ordinary correctness bugs and documentation problems can use the public issue tracker.

## Deployment boundary

`matrix-server` does not authenticate MCP clients. Operators must restrict the listener
to trusted clients or place it behind an authenticated proxy or gateway. The default
standalone listener and the example Compose published port both use loopback.

Only Linux deployments apply the decoder's address-space and privilege-gain limits.
Use the supplied Linux container when accepting media from callers you do not fully
trust, and keep FFmpeg updated through rebuilt images.
