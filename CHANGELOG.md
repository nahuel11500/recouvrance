# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.0](https://github.com/nahuel11500/recouvrance/compare/v0.1.0...v0.2.0) - 2026-10-22

### Added

- Delete confirmation per kind, with defaults by how much a kind matters ([#16](https://github.com/nahuel11500/recouvrance/pull/16))
- A simpler app icon for 16 and 24 px ([#15](https://github.com/nahuel11500/recouvrance/pull/15))
- New app icon, with a README trust section, screenshots and tour, pinned actions and Scorecard ([#14](https://github.com/nahuel11500/recouvrance/pull/14))
- *(agents)* Activity grouped by task, with filters and older calls ([#13](https://github.com/nahuel11500/recouvrance/pull/13))
- *(pods)* Port-forward from the Ports cards ([#10](https://github.com/nahuel11500/recouvrance/pull/10))
- *(nodes)* Allocation as cards with requests and limits bars ([#9](https://github.com/nahuel11500/recouvrance/pull/9))

### Fixed

- Run each exec-plugin login once at a time ([#11](https://github.com/nahuel11500/recouvrance/pull/11))
- Let dialog inputs keep focus ([#8](https://github.com/nahuel11500/recouvrance/pull/8))
- *(table)* One sort arrow, on the sorted column only ([#6](https://github.com/nahuel11500/recouvrance/pull/6))

## [0.1.0](https://github.com/nahuel11500/recouvrance/compare/v0.0.1...v0.1.0) - 2026-10-20

### Added

- *(namespaces)* Larger picker and frequently visited namespaces ([#4](https://github.com/nahuel11500/recouvrance/pull/4))
- Show the version, add --version and --help, cross-build Intel Macs ([#3](https://github.com/nahuel11500/recouvrance/pull/3))

## [0.0.1]

### Added

- Harbour look, after the schooner Recouvrance: navy hull sidebar, sail-canvas pages, brass accent
  (selection, primary buttons), sea and signal status colors; brass, harbour, sea, kelp, signal and
  violet accents.
- CPU and memory history per container, against its request and limit.
- Secret values revealed one at a time by clicking them, with a copy button.
- One-click port-forwards open the browser on web ports (setting); recent forwards in the sidebar
  start again in one click.
- GPU usage (utilization and memory) next to CPU and memory on pod, workload and node pages and the
  cluster overview, from NVIDIA's DCGM exporter through Prometheus or VictoriaMetrics; GPU requests
  against allocatable GPUs on the cluster overview.
- Diagnosis of broken pods and workloads (crash output, exit codes, scheduling, image pulls,
  probes, volumes).
- Roll back Deployments, StatefulSets and DaemonSets to a revision, with diff and server dry run.
- Attach to a container's process; copy files in and out of containers; bulk scale.
- Edit ConfigMap and Secret values as key/value rows; create Secrets and ConfigMaps from forms.
- RBAC pages (roles, bindings, ServiceAccounts), a who-can inspector, short-lived ServiceAccount
  kubeconfigs; PodDisruptionBudget and VerticalPodAutoscaler pages.
- Node allocation (extended resources included), filesystems, DRA devices; pod ephemeral storage.
- Create / apply: templates and your own templates folder, files, URLs, drag and drop, `$EDITOR`,
  errors on their line, field docs and forms from the cluster's schema.
- Find in logs (regex, match case); exclusions in the log filter.
- Table filter expressions on columns, filter history and saved searches; links from cells to the
  objects they name; tabs; CSV export; click a namespace to filter.
- Port-forward options (local port, listen address, https, open in browser); forwards follow
  replacement pods.
- VictoriaMetrics support; per-cluster settings (metrics endpoint, node shell image, accessible
  namespaces); manually added namespaces.
- Kubeconfig watched folders, reload on change, contexts with the same name kept apart.
- Accent color, metrics off switch, refresh (F5), sidebar toggle (Ctrl+B).

### Changed

- Types you may not list (403) stop retrying and say so; API errors are shown as the API's own message.
- Responses from the API server are gzip-compressed (much faster lists on remote clusters).
- The in-app assistant runs through Claude Code (your subscription) when it is installed, and
  needs an Anthropic API key only without it.
