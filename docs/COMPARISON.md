# Recouvrance vs Aptakube, FreeLens and Headlamp

Sources checked on 2026-11-21: [Aptakube's changelog](https://aptakube.com/changelog) (to 1.23.0),
its [GitHub releases](https://github.com/aptakube/aptakube/releases) and its site's docs and feature
pages; [FreeLens's release notes](https://github.com/freelensapp/freelens/releases) (to v2.1.0), its
issues, its code and its extensions' READMEs; [Headlamp's release notes](https://github.com/kubernetes-sigs/headlamp/releases)
(to v0.46.0), its code, and the READMEs and code of its [plugins](https://github.com/headlamp-k8s/plugins).
FreeLens and Headlamp were not installed and run here.

How Recouvrance compares, feature by feature, with other Kubernetes desktop clients. Based on
their release notes, documentation and issue trackers; "–" means not offered. Corrections are
welcome: open an issue or a pull request.

| Area | Aptakube | FreeLens | Headlamp | Recouvrance |
|---|---|---|---|---|
| Stack | Tauri (Rust + web UI) | Electron | Web / Electron | Native Rust (gpui), no web view |
| Several clusters at once | ✓ | – | ✓ incl. side-by-side cluster comparison | ✓ merged tables with a Context column; compare an object across clusters |
| Live tables with kubectl's columns, CRDs | ✓ | ✓ | ✓ | ✓ server-side tables; 20,800 pods listed in 1.4 s |
| Filter | text, `!not`, either-or terms, wildcards, `/regex/`, labels | text | text; Advanced Search with expressions, saved queries | text, `!not`, `label=value`, `ns:glob`, `/regex/`, column expressions (`restarts>3`, `cpu>200m`, `age<1h`), history and saved searches |
| Saved views / workspaces | workspaces (sets of contexts, connected at once) | hotbar | projects (resources grouped across namespaces and clusters) | saved views (type + namespaces + filter), opened on the contexts in use; no saved sets of contexts |
| Pod logs, several pods merged | ✓ level filter, histogram, regex; several workloads at once | ✓ | ✓ | ✓ timeline, level filter, histogram, wrap, readable JSON, find with regex |
| Diagnosis (why a pod or workload is broken) | – | – | ✓ hints from status, events and containers on pod and workload pages | ✓ crash output and exit code, scheduling, image pulls, probes, volumes |
| Roll back to a revision | ✓ Deployments, StatefulSets, DaemonSets | – | ✓ Deployments, StatefulSets, DaemonSets; dry run shown as YAML | ✓ Deployments, StatefulSets, DaemonSets, with diff and dry run |
| Attach / copy files | ✓ attach; download files and folders | attach | attach | ✓ attach to the main process; download and upload through the container |
| Browse container files | ✓ Files tab: browse, preview text, download (folders as .tar) | – | – | ✓ browse, preview, download (folders as .tar), search by name; distroless through a debug container, containers not running through a copy of the pod |
| Shell into containers | ✓ | ✓ | ✓ | ✓ plus local shell with kubectl on the context |
| Ephemeral debug containers | ✓ custom image, profiles | ✓ image, profile and spec in a dialog | ✓ with a target container | ✓ with a Pod Security "restricted" fallback |
| Node shell | ✓ | ✓ | ✓ | ✓ (privileged pod, confirmed, cleaned up) |
| Port-forward | ✓ saved, follows replacement pods | ✓ (listen address, SOCKS5) | ✓ Service forwards reconnect to new pods | ✓ remembered and restorable, follows replacement pods; listen address, https, open in browser; through the kubeconfig's `proxy-url` (SOCKS5 or HTTP) |
| Edit YAML | ✓ warns of changes made meanwhile; shows the JSON patch | ✓ | ✓ dry run; YAML errors on their line; diff before saving; warns of changes made meanwhile | ✓ validated by a server dry run first; errors on their line; field docs and forms from the cluster's schema |
| Edit ConfigMap/Secret values | ✓ | ✓ | ✓ without the YAML editor; ConfigMap `binaryData` too | ✓ as key/value rows; Secret forms (Opaque, registry, TLS, basic auth) |
| Create / apply YAML | ✓ | ✓ | ✓ forms for common kinds; load from a URL | ✓ multi-document, dry run; templates, files, URLs, `$EDITOR` |
| Compare objects | ✓ across clusters or namespaces | – | – | ✓ across clusters or objects; revision diffs |
| History of changes | workload revisions; Helm release history | Deployment revisions (its ReplicaSets); Helm revisions | workload revision history; Helm release revisions (plugin) | ✓ per object: revisions, Helm, Argo CD deployments, field managers, events; a Timeline recorded across clusters |
| Scale / restart / delete / cordon / drain / run CronJob | ✓ plus bulk actions, across clusters | ✓ | ✓ plus bulk delete and scale, evict | ✓ plus bulk actions, force delete, finalizer removal |
| Shows the kubectl equivalent | – | – | – | ✓ on every confirmation |
| Protected / colored contexts | ✓ protected or read-only; color, icon, tags | icon color and label per cluster | icon and color per cluster | ✓ colors; protected contexts wait a few seconds or ask for the name before writes; production names guessed |
| Actions match your RBAC | – | – | ✓ | ✓ actions you may not do are disabled; "My access" view |
| RBAC pages | lists Roles, bindings and ServiceAccounts | ✓ | ✓ | ✓ rules, bindings, ServiceAccounts' effective access, who-can, short-lived ServiceAccount kubeconfig |
| Node allocation, GPUs, DRA | CPU and memory requests vs allocatable | ✓ | DRA devices (alpha); extended resources such as GPUs in node details | ✓ requests/limits vs allocatable incl. extended resources; DRA devices |
| Metrics now | ✓ CPU/memory bars with request and limit markers | ✓ CPU/memory columns in pod tables | ✓ CPU/memory columns in pod tables, % of request and limit on hover | ✓ CPU/memory columns with bars against requests and limits (pods) or allocatable (nodes) |
| Usage history | – | ✓ Prometheus | plugin | ✓ Prometheus or VictoriaMetrics, found automatically via the API server proxy; 15m–7d; cluster, nodes, workloads, pods |
| Request efficiency | now, not over time: near limit, over request, unused requests | per-container charts: usage vs requests and limits over time (Prometheus) | – | ✓ used vs requested over time |
| Problems view | Health page: pod status, recent restarts and warnings | overview warnings: node conditions, Warning events | events on the cluster overview; warning counts on Home | ✓ across clusters: crash loops, pending, unavailable, NotReady, failed jobs, missing references, stuck deletions |
| Firing alerts | – | – | – | ✓ from the cluster's Prometheus, on Problems and on the pages of what they are about; silenced ones muted |
| Upgrade check | – | – | – | ✓ what to fix before the next minor version: removed APIs still used (objects, Helm releases, the API server's metric), version skew, drains that would block, feature gates |
| Certificates | ✓ TLS Secrets decoded; cert-manager pages | ✓ certificates in Secrets, with expiry | plugin (cert-manager) | ✓ decoded from Secrets/ConfigMaps, expiry |
| Gateway API pages | ✓ | extension | ✓ | ✓ |
| Reachability (why traffic doesn't reach the pods) | links between Ingresses, routes, Services and pods | a Service's endpoints | pods a Service selects | ✓ follows Ingress or route, Service, ports, pods, endpoints, TLS; names the first broken step; test from this machine |
| Flux / Argo CD actions | Argo CD sync (prune optional) | extension (Flux) | plugins | ✓ reconcile, suspend, refresh, sync |
| Argo CD page and diagnosis | Applications with sync and health | – | plugin: Applications, sync, refresh, rollback; checks the APIs of managed resources are served | ✓ every Application of every hub, under its app of apps; why a sync failed, sync loops, stuck deletions, out-of-sync fields as a diff |
| Helm releases | ✓ | ✓ | plugin | ✓ values (masked), manifest, history, upgrade with edited values, roll back, uninstall |
| Helm install from repos | – | ✓ | plugin | ✓ search your repos, pick a version, edit values, server dry run |
| Map / relations | links between related objects | extension | ✓ | ✓ per object |
| Command palette | ✓ | ✓ | ✓ global search: objects, clusters, pages | ✓ across clusters |
| Keyboard navigation | ✓ | partial | partial | ✓ ↑ ↓ Enter Esc / Ctrl+K |
| Multiple windows / tabs | ✓ both | dock tabs only (terminals, logs, editors) | activities (details, logs, shells, editors) | ✓ both |
| Kubeconfig folders, reload on change | folders, reloaded with a button | ✓ | ✓ | ✓ contexts with the same name kept apart |
| AI | – | extensions: AI chat, Claude chat, coding agents in a terminal | plugin: assistant chat, diagnosis of warning events, MCP servers as tools (desktop) | ✓ assistant chat beside any page (Claude, through your Claude Code login or an API key), under the agents' rules; an MCP server and prompts for your own agents |
| Agent safety | – | extensions: writes approved one by one; a read-only mode (Claude); command allow and ask lists (AgentBridge) | plugin: tool calls approved in a dialog, or a tool or the session auto-approved | ✓ per-agent rules (allow, ask, deny) by verb, resource, name, namespace and context; approvals in the app; rate limit, kill switch, audit log |
| Extensions / plugins | – | ✓ (JS extensions; a new API in v2) | ✓ (JS plugins) | ✓ command plugins (YAML: your commands on resources, in a terminal or background) |
| Web version | – | – | ✓ | – |

Not in the table: [k9s](https://github.com/derailed/k9s), a terminal UI; [Lens](https://lenshq.io), the
desktop app by Mirantis; and [Radar](https://github.com/skyhook-io/radar), an open-source UI in one Go
binary (in a browser, as a desktop app or in-cluster) with an issues queue, topology, a timeline of
changes with diffs, upgrade impact and an MCP server for agents: a read-only endpoint, or a full one
whose write tools are marked for the agent's client to confirm.

## Measured

Recouvrance v0.9.0, release build with the `dev` feature (for its timing hooks), on a laptop, on
2026-11-21. Read-only, against a production cluster with about 20,800 pods. Memory is the process's
peak resident memory (VmHWM).

| Scenario | Time | Peak memory |
|---|---|---|
| List every pod (`RECOUVRANCE_SMOKE=<context>`, the window's list path; median of 3 runs) | first rows 0.7 s, all rows 1.4 s, complete with their metadata 4.2 s | 182 MB |
| Problems scan of that cluster (`RECOUVRANCE_PROBLEMS=1`, 2 runs) | 6 s | 1.2 GB ([#842](https://github.com/nahuel11500/recouvrance/issues/842)) |
| Workloads overview of that cluster (`RECOUVRANCE_DASH=1`) | 0.9 s | 212 MB |
| The app on the Pods page, all namespaces, for 2 minutes (`--snapshot`, drawn off screen at 2×) | | 410–440 MB once loaded; 1.5 GB in the first seconds, while the Problems scan runs |
| A frame of a 20,242-pod table (kwok on kind, `RECOUVRANCE_FRAMES_END`: redraws forced, rows not changing) | 8.2 ms median, 9.1 ms p90 | |

The smoke runs have no window, so their memory leaves out drawing.

On 2026-10-16, with Recouvrance 0.0.1, read from the process table while both were in normal use:
Aptakube used 4.0 GB plus 1.35 GB for its WebKit view; Recouvrance used 164 MB, 5 minutes after
starting. Not a controlled benchmark: the clusters and pages each had open are not known.

## Not offered

- UI extensions that draw their own pages (FreeLens/Headlamp run JavaScript for that). Recouvrance's
  plugins run commands instead, and the MCP server lets agents and scripts drive it.
- A web mode, for clusters reached through a browser only. Not planned: Recouvrance is a native app
  on purpose (no web view).
- Windows. Recouvrance is built for Linux and macOS; Aptakube, FreeLens and Headlamp also run on
  Windows.
