# Security policy

## Reporting a vulnerability

Email **hello@crowdedkingdoms.com** with the subject line starting `SECURITY:`, a description, the
affected version (a git tag, an npm or crates.io version) and steps to reproduce. Do not open a
public issue for a vulnerability. You will get an acknowledgement within two business days and a
status update at least every week until it is resolved.

## What is in scope

- The `ckx-kit` command and what it runs on your machine.
- The `ckx-sdk` and `crowdy-client-sdk` crates.
- Anything here that would let a crate pass these checks and then do on the platform what the
  platform's own build would refuse.

The platform rebuilds every mod from its source and checks it again in its own sandbox, so a
local build is never what runs for players.
