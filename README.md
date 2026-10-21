<h1 align="center">
  <img src="assets/recouvrance.svg" width="112" alt=""><br>
  Recouvrance
</h1>

[![CI](https://github.com/nahuel11500/recouvrance/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/nahuel11500/recouvrance/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/nahuel11500/recouvrance/badge)](https://scorecard.dev/viewer/?uri=github.com/nahuel11500/recouvrance)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

*Named after the Brest schooner* Recouvrance*, a replica of an 1817 aviso: the fast ships that carried
dispatches. In French,* recouvrer *means to recover.*

A fast Kubernetes desktop client (Rust + [gpui](https://github.com/zed-industries/zed)) with a
**local MCP server**, so AI agents can work on your clusters under rules you control.

<div align="center">
  <img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/recouvrance-tour.gif" width="85%" alt="A tour of Recouvrance: the pods table, the command palette, a crash diagnosis, logs across pods, the Problems page and the workloads overview">
</div>

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

## Screenshots

<table>
  <tr>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/pods.png" alt="The pods table across namespaces, with status, CPU and memory"></td>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/pods-dark.png" alt="The same table in the dark theme"></td>
  </tr>
  <tr>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/diagnosis.png" alt="A crash-looping pod with its diagnosis and last output"></td>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/logs.png" alt="Logs of every pod of a deployment, merged, with levels and a timeline"></td>
  </tr>
  <tr>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/command-palette.png" alt="The command palette (Ctrl+K) finding objects and pages"></td>
    <td width="50%"><img src="https://raw.githubusercontent.com/nahuel11500/recouvrance/media/screenshots/workloads.png" alt="The workloads overview with warnings and recent restarts"></td>
  </tr>
</table>

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

## Trust and security

Recouvrance holds your cluster credentials and can let AI agents act on your clusters, so here is
what you can check for yourself:

- **No telemetry.** The app talks to your clusters, to the Prometheus or VictoriaMetrics you point
  it at, and to the Anthropic API only if you use the assistant. Nothing else, and no analytics.
- **Safe by default.** Read-only until you turn writes on; every write shows its `kubectl`
  equivalent, runs as a server dry run first, and asks for confirmation (and for the context's name
  on protected contexts). Agents get only what their rules allow, every call is logged to
  `~/.local/share/recouvrance/audit.jsonl`, and a kill switch cuts them all off.
- **Verifiable releases.** Every release archive, `.deb` and AppImage has a build provenance
  attestation: `gh attestation verify <file> --repo nahuel11500/recouvrance` proves it was built
  by this repository's release workflow from the tagged commit.
- **Checked dependencies.** [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) checks every
  change, and again every week, against the RustSec advisory database and the allowed licenses. Dependabot
  proposes updates only after a two-week cooldown, so a compromised release is usually caught first.
- **Hardened CI.** Every GitHub Action is pinned to a commit. Workflow tokens are read-only or
  empty, except the release workflow's, which uploads the binaries, and the release secrets live in
  a protected environment. The
  [OpenSSF Scorecard](https://scorecard.dev/viewer/?uri=github.com/nahuel11500/recouvrance) above
  scores these practices weekly.
- **Tested changes.** `main` only changes through pull requests that pass CI: format, clippy,
  tests, spelling, licenses and advisories. Each push to `main` then also runs the macOS tests,
  end-to-end tests against a kind cluster, the minimum Rust version and release builds. Release tags
  are protected, and releases are cut by [release-plz](https://release-plz.dev) from the changelog.
- **Private reporting.** Report vulnerabilities through GitHub's private reporting, as described in
  [SECURITY.md](SECURITY.md).

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
