# Recouvrance vs Aptakube, FreeLens and Headlamp

Sources checked on 2026-10-16: the projects' READMEs and docs, FreeLens release notes (v1.10.3),
the Kubernetes blog posts on Headlamp (2025 highlights, Dashboard → Headlamp), and guides about
Headlamp v0.41. FreeLens and Headlamp were not installed and run here.

How Recouvrance compares, feature by feature, with other Kubernetes desktop clients. Based on
their public documentation and issue trackers; "–" means not offered. Corrections are welcome:
open an issue or a pull request.

| Area | Aptakube | FreeLens | Headlamp | Recouvrance |
|---|---|---|---|---|
| Stack | Tauri (Rust + web UI) | Electron | Web / Electron | Native Rust (gpui), no web view |
| Several clusters at once | ✓ | – | ✓ incl. side-by-side cluster comparison | ✓ merged tables with a Context column; compare an object across clusters |
| Live tables with kubectl's columns, CRDs | ✓ | ✓ | ✓ | ✓ server-side tables, paged, 24k pods in ~170 MB |
| Filter | text | text | text | text, `!not`, `label=value`, `ns:glob`, `/regex/`, column expressions (`restarts>3`, `cpu>200m`, `age<1h`), history and saved searches |
| Saved views / workspaces | – | hotbar | projects (resources grouped across namespaces and clusters) | saved views (type + namespaces + filter, one or more clusters) |
| Pod logs, several pods merged | ✓ | ✓ | ✓ | ✓ timeline, level filter, histogram, wrap, readable JSON, find with regex |
| Diagnosis (why a pod or workload is broken) | – | – | – | ✓ crash output and exit code, scheduling, image pulls, probes, volumes |
| Roll back to a revision | – | – | – | ✓ Deployments, StatefulSets, DaemonSets, with diff and dry run |
| Attach / copy files | – | – | – | ✓ attach to the main process; download and upload through the container |
| Shell into containers | ✓ | ✓ | ✓ | ✓ plus local shell with kubectl on the context |
| Ephemeral debug containers | – | – | – | ✓ with a Pod Security "restricted" fallback |
| Node shell | – | ✓ | – | ✓ (privileged pod, confirmed, cleaned up) |
| Port-forward | ✓ (remembered) | ✓ (listen address, SOCKS5) | ✓ | ✓ remembered and restorable, follows replacement pods; listen address, https, open in browser; SOCKS5/HTTP proxy |
| Edit YAML | ✓ | ✓ | ✓ | ✓ validated by a server dry run first; errors on their line; field docs and forms from the cluster's schema |
| Edit ConfigMap/Secret values | – | ✓ | – | ✓ as key/value rows; Secret forms (Opaque, registry, TLS, basic auth) |
| Create / apply YAML | ✓ | ✓ | ✓ | ✓ multi-document, dry run; templates, files, URLs, `$EDITOR` |
| Compare objects | ✓ diff | – | – | ✓ across clusters or objects; revision diffs |
| Scale / restart / delete / cordon / drain / run CronJob | ✓ | ✓ | ✓ | ✓ plus bulk actions, force delete, finalizer removal |
| Shows the kubectl equivalent | – | – | – | ✓ on every confirmation |
| Protected / colored contexts | – | – | – | ✓ type the context name before writes |
| Actions match your RBAC | – | – | ✓ | ✓ actions you may not do are disabled; "My access" view |
| RBAC pages | – | ✓ | ✓ | ✓ rules, bindings, ServiceAccounts' effective access, who-can, short-lived ServiceAccount kubeconfig |
| Node allocation, GPUs, DRA | – | ✓ | – | ✓ requests/limits vs allocatable incl. extended resources; DRA devices |
| Metrics now | metrics-server | ✓ CPU/memory columns in pod tables | Prometheus plugin | ✓ CPU/memory columns with bars vs node capacity |
| Usage history | – | ✓ Prometheus | plugin | ✓ Prometheus or VictoriaMetrics, found automatically via the API server proxy; 15m–7d; cluster, nodes, workloads, pods |
| Request efficiency | – | – | – | ✓ used vs requested over time |
| Problems view | – | – | – | ✓ crash loops, pending, unavailable, NotReady, failed jobs |
| Certificates | – | – | – | ✓ decoded from Secrets/ConfigMaps, expiry |
| Gateway API pages | – | – | ✓ | ✓ |
| Flux / Argo CD actions | – | extension | plugin | ✓ reconcile, suspend, refresh, sync |
| Helm releases | ✓ | ✓ | plugin | ✓ values (masked), manifest, history, upgrade with edited values, roll back, uninstall |
| Helm install from repos | – | ✓ | plugin | ✓ search your repos, pick a version, edit values, server dry run |
| Map / relations | – | – | ✓ | ✓ per object |
| Command palette | ✓ | ✓ | – | ✓ across clusters |
| Keyboard navigation | ✓ | partial | partial | ✓ ↑ ↓ Enter Esc / Ctrl+K |
| Multiple windows / tabs | – | ✓ | – | ✓ both |
| Kubeconfig folders, reload on change | – | ✓ | ✓ | ✓ contexts with the same name kept apart |
| AI | – | – | ✓ assistant chat in the UI (uses MCP) | ✓ assistant chat in the app (Claude, through your Claude Code login or an API key) that works as an MCP agent under your rules, approvals and audit log; plus the MCP server for your own agents (Claude Code…) and prompts |
| Extensions / plugins | – | ✓ (JS extensions) | ✓ (JS plugins) | ✓ command plugins (YAML: your commands on resources, in a terminal or background) |
| Web version | – | – | ✓ | – |

## Measured

On the same laptop (2026-10-16), read from the process table while both were in normal use:
Aptakube used 4.0 GB plus 1.35 GB for its WebKit view; Recouvrance used 164 MB, 5 minutes after
starting. Not a controlled benchmark: the clusters and pages each had open are not known.

Recouvrance on its own (release build, metrics read from the process table):

| Scenario | Time | Peak memory |
|---|---|---|
| List 23,958 pods (a large production cluster) | 17 s (the API server's time) | 174 MB |
| Problems scan of that cluster | 31 s | 250 MB |
| Workloads overview on that cluster, 4 minutes (debug build) | refreshes every ~60 s | flat around 0.9 GB |

## Not offered

- UI extensions that draw their own pages (FreeLens/Headlamp run JavaScript for that). Recouvrance's
  plugins run commands instead, and the MCP server lets agents and scripts drive it.
- A web mode, for clusters reached through a browser only. Not planned: Recouvrance is a native app
  on purpose (no web view, low memory).
