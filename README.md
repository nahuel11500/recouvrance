# Recouvrance

[![CI](https://github.com/nahuel11500/recouvrance/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/nahuel11500/recouvrance/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

*Named after the Brest schooner* Recouvrance*, a replica of an 1817 aviso: the fast ships that carried
dispatches. In French,* recouvrer *means to recover.*

A fast Kubernetes desktop client (Rust + [gpui](https://github.com/zed-industries/zed)) with a
**local MCP server**, so AI agents can work on your clusters under rules you control.

<!-- Screenshot, once docs/images/recouvrance.png is committed:
![Recouvrance: the pods table of a cluster, with the sidebar and a pod's detail page](docs/images/recouvrance.png)
-->

## Highlights

- **Native and fast**: Rust and gpui, no web view. Server-side tables keep clusters with tens of
  thousands of pods in a few hundred megabytes.
- **Everything in one place**: live tables for every type (CRDs included), logs across pods with
  find and filters, shell, attach, file copy, port-forwards, YAML with field docs and schema forms,
  Helm, Flux and Argo CD actions, several clusters at once, tabs and windows.
- **Tells you what is wrong**: a Problems page across clusters, a diagnosis on every broken pod or
  workload, usage history from Prometheus or VictoriaMetrics, certificate expiry.
- **Safe by default**: read-only until you enable writes; every write is confirmed with its
  `kubectl` equivalent and checked with a server dry run; protected contexts; actions follow your
  RBAC; roll back any Deployment, StatefulSet or DaemonSet.
- **Access made clear**: Role, binding and ServiceAccount pages, a *who can* inspector, and
  short-lived kubeconfigs for ServiceAccounts.
- **Built for agents**: a local MCP server so Claude Code (or any MCP client) works on your clusters
  under rules you control (allow, ask, deny), with approvals and an audit log. The in-app assistant
  uses your Claude Code login (no API key needed), or an Anthropic API key without Claude Code.

## Install

Linux (x86_64, arm64) and macOS (Intel, Apple silicon):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nahuel11500/recouvrance/releases/latest/download/recouvrance-installer.sh | sh
recouvrance-update      # later: installs the latest release
```

Or download from the [latest release](https://github.com/nahuel11500/recouvrance/releases/latest):
a tarball for each platform, or for x86_64 Linux only a `.deb` (Debian, Ubuntu) and an AppImage.
From source with Rust: `cargo install --locked --git https://github.com/nahuel11500/recouvrance` (on Linux, the libraries listed in
[development](docs/development.md#build) first).

macOS builds aren't notarized. The install script above is not affected; a tarball downloaded with
a browser is quarantined: the first time, run `xattr -d com.apple.quarantine path/to/recouvrance`
or allow it in System Settings → Privacy & Security.

The tarballs, the `.deb` and the AppImage have build provenance attestations, which show they were
built by this repository's release workflow from the tagged commit. With the GitHub CLI:

```sh
gh attestation verify recouvrance-x86_64-unknown-linux-gnu.tar.xz --repo nahuel11500/recouvrance
```

From a checkout:

```sh
scripts/install.sh      # builds, installs ~/.local/bin/recouvrance, adds it to your app menu
scripts/uninstall.sh    # removes it (settings and the agent log are kept)
```

## Quick start

1. Launch Recouvrance and pick one or more contexts from your kubeconfig.
2. Browse with the sidebar or **Ctrl+K**; filter tables with words, labels or expressions such as
   `status:Running restarts>3`.
3. Click the `read-only` badge when you want to change something.
4. In **Agents**, click **Connect Claude Code** to let Claude work on your clusters under your rules.

| Shortcut | Action |
|---|---|
| Ctrl+K | Command palette: resources, pages, contexts, objects |
| Ctrl+T / Ctrl+W / Ctrl+Tab | New tab / close tab / next tab |
| Ctrl+F or / | Filter the current table |
| Alt+← / Alt+→ | Back / forward (Esc also goes back from a detail page) |
| Ctrl+Shift+N | New window |

## Documentation

[Features](docs/features.md) · [Assistant and agents](docs/agents.md) ·
[Configuration](docs/configuration.md) · [Architecture](docs/architecture.md) ·
[Development](docs/development.md) · [Comparison with other clients](docs/COMPARISON.md)

## Contributing

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). Please follow the
[code of conduct](CODE_OF_CONDUCT.md), and report security issues as described in
[SECURITY.md](SECURITY.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in Recouvrance by you, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
