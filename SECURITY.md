# Security policy

Recouvrance holds access to your clusters and can give AI agents access to them, so security
reports are taken seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Use GitHub's private vulnerability reporting: the
**Security** tab of the repository → **Report a vulnerability**. Include what you found, how to
reproduce it, and its impact.

You will get an answer within a few days. Once a fix is released, you are credited in the release
notes unless you prefer otherwise.

## Scope

In scope, for example:

- an agent getting around its rules, approvals, rate limit or the kill switch;
- the MCP server reachable from outside `127.0.0.1`, or usable without a valid token;
- secrets (Secret values, tokens, kubeconfig credentials) written to disk or logs unprotected;
- a write happening without the confirmation the UI shows.

Out of scope: what your own kubeconfig credentials allow (Recouvrance acts with them), and agents
that run `kubectl` directly with your kubeconfig instead of going through Recouvrance.

## Supported versions

Only the latest release is supported: security fixes go into a new release, not into older ones.
Update with `recouvrance-update` (install script), `cargo install --locked recouvrance`, or the
new release's `.deb`, AppImage or tarball.
