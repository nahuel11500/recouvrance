# Security policy

Recouvrance holds access to your clusters and can give AI agents access to them, so security
reports are taken seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately with GitHub's vulnerability reporting
form: <https://github.com/nahuel11500/recouvrance/security/advisories/new> (the **Security** tab of
the repository → **Report a vulnerability**). Include what you found, how to reproduce it, and its
impact.

What happens next:

- you get a first answer within 7 days;
- the fix goes into a new release as soon as it is ready;
- the advisory is published (disclosure) once the fix is released, and at the latest 90 days after
  your report, unless we agree on another date together;
- you are credited in the advisory and the release notes, unless you prefer otherwise.

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
Update with `recouvrance-update` (install script), the new release's `.deb`, AppImage or tarball,
or from source with `cargo install --locked --git https://github.com/nahuel11500/recouvrance --tag vX.Y.Z`.
Recouvrance is not on crates.io: a `recouvrance` crate there is not this project.

## Security fixes in the release notes

Each release lists its security fixes first, under **Security**: when a release has any, update.
Issues found by our own reviews and audits are tracked in the open, as issues labelled
[`security`](https://github.com/nahuel11500/recouvrance/issues?q=label%3Asecurity), and get no
advisory; a privately reported vulnerability gets an advisory once its fix is released (above).
