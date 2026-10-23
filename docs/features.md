# Features

- **Contexts**: pick one or several on launch; tables merge clusters with a Context column.
- **Live tables** with kubectl's own columns for every type, including CRDs; CPU/memory from
  metrics-server; per-type column choice; multi-namespace filter.
- **Detail pages** for pods (per-container tabs: probes, usage vs requests/limits, ports, env,
  mounts), workloads (scheduling, pods, revisions with diffs), services (port-forward, backing
  pods), secrets (masked, referenced by), ingresses, PVCs, namespaces (quotas), nodes, CronJobs,
  and a generic spec/status view for anything else.
- **GPU usage**: utilization and memory per pod, workload, node and cluster, from NVIDIA's DCGM
  exporter (shown when its metrics are in Prometheus or VictoriaMetrics).
- **Usage history** (last hour of CPU and memory against requests and limits) for pods and workloads, from the cluster's Prometheus, found automatically and read through the API server proxy. A workload's charts cover up to 300 of its pods, and its request and limit lines the same pods.
- **Logs** across all pods of a workload (up to 50, running and newest first), level filter, time range, timeline histogram.
- **Shell** (exec with a real terminal), **node shell** (privileged pod, deleted when its shell
  ends or its page goes away) and **port-forward** for pods and services; forwards are remembered
  and restored with one click. Shells and attach need write mode (they run commands in the
  cluster), and the context name typed on a protected context.
- **RBAC-aware**: actions your Kubernetes RBAC forbids are disabled; *My access* shows a can-i
  matrix for the namespace.
- **Problems**: one list of what is broken across clusters (crash loops, pending pods, unavailable workloads, NotReady nodes, failed jobs, pending volumes).
- **Overviews**: cluster capacity and health, workloads status, Helm releases (values masked; install from your repos, roll back and uninstall through your `helm` CLI).
- **API requests** (Settings → API requests): per cluster, how many requests of each kind
  (discovery, lists, watches, metrics, logs, shells, writes) were made, how many failed, and how
  long the API server took to start answering: average, slowest, last.
- **Favorites** and **saved views** (type + namespaces + filter, bookmark button), back/forward history, light and dark themes.
- **Instant back**: the last 8 lists (up to 10,000 rows in all) are kept, so going back to one
  shows it at once, marked "Updating…", while a fresh list loads and replaces it in one go.
- **Bulk actions**: check rows, then restart, scale, delete or copy names. Scaling needs the number
  of objects typed (the replica count is never suggested), deleting asks as set for the kind (see
  below), and each protected context among them needs its name typed; rows from several contexts
  show which one they are on.
- **Delete confirmation per kind** (Settings → General → Deleting): delete right away, ask, or
  type the name (the count, for several). By default, what comes back on its own (Pods, Jobs,
  ReplicaSets, Events…) is deleted right away; what takes data, machines or everything inside with
  it (Nodes, Namespaces, StatefulSets, Deployments, DaemonSets, PersistentVolumes and their claims,
  CRDs) needs the name typed; anything else, custom resources included, asks. A kind is added by
  its name, plural or short name (`deploy`), checked against the connected clusters. Every delete dialog
  links to the setting. A protected context always asks for its name, and an object stuck
  terminating always shows the dialog, to remove its finalizers.
- **Write mode** (off by default): click the `read-only` badge to enable scale, restart, delete
  (confirmed as set per kind), YAML edits (validated by a server dry run first), cordon/uncordon nodes,
  debug pods with an ephemeral container (falls back to a Pod Security "restricted" profile), run a CronJob now, suspend/resume (CronJobs, Flux, Argo CronWorkflows), drain nodes, force delete
  or remove finalizers (only once the delete leaves it stuck terminating: it already was, or still is
  after 10 seconds), Flux *Reconcile*, Argo CD
  *Refresh*/*Sync*, create/apply YAML (a server dry run first, then a confirmation listing each
  object's kind, namespace and name). YAML edits on a detail page save only that object: a changed
  kind, API group, namespace or name is refused, and so are masked Secret values. Every
  confirmation shows the equivalent `kubectl` command. A page whose object is deleted says so and
  stops polling.
- **Plugins**: your own commands on resources (`~/.config/recouvrance/plugins.yaml`):
  `$CONTEXT`, `$NAMESPACE`, `$NAME`, `$KIND` are filled in and kubectl points at the cluster; they
  run in a terminal page or in the background. A plugin counts as a write (write mode, protected
  context name typed) unless it declares `read_only: true`. See Settings → Plugins for an example.
- **Contexts** can get a color (a strip at the top of the window) and be marked *protected*: writes
  there ask you to type the context name. Contexts you have not set are guessed from the context,
  user and cluster names and the server URL (`prod`, `prd`, `production`, `live`, `admin`, `root`),
  so new production contexts are protected too. Set both in Settings → Contexts.
- **Diagnosis** on pods and workloads: why it is broken (crash output and exit code, scheduling,
  image pulls, probes, volumes) and what to try.
- **Rollback** of Deployments, StatefulSets and DaemonSets to any revision, with the diff and a
  server dry run first; **attach** to a container's process; **file copy** in and out of containers;
  **bulk scale**.
- **Configuration**: edit ConfigMap/Secret values as key/value rows; create Secrets (Opaque, registry,
  TLS, basic auth) and ConfigMaps from forms.
- **Access**: Role, binding and ServiceAccount pages (rules, who is bound, add/remove subjects, bind a
  role; a binding changed meanwhile makes the change fail instead of being overwritten), a *who can*
  inspector, and a kubeconfig for a ServiceAccount (token valid 1 hour by default, 24 at most,
  saved to ~/Downloads readable only by you, never overwriting a file). PDB and VPA pages.
- **Nodes**: requests/limits against allocatable (GPUs and other extended resources included),
  filesystems, DRA devices; ephemeral storage per pod.
- **YAML**: templates (plus your own in `~/.config/recouvrance/templates/`), open/drop files, load an
  https URL, edit in `$EDITOR`, errors located on their line, field docs (`kubectl explain`) and forms
  generated from the cluster's schema, managedFields toggle, download as YAML.
- **Logs**: find (regex, match case, next/previous) on top of the filter (`!word` excludes).
- **Tables**: column expressions in the filter (`status:Running restarts>3 cpu>200m age<1h`), hidden
  columns included (`ip:10.4.2.17`), filter history and saved searches, links from cells to the
  objects they name, click a namespace to filter, CSV export, tabs.
- **Port-forwards** with options: local port, listen address, https, open in the browser; they follow
  replacement pods.
- **Per-cluster settings**: Prometheus or VictoriaMetrics endpoint, node shell image, accessible
  namespaces (when you may not list them), metrics off; namespaces can also be added by hand.
- **Kubeconfig**: extra files and watched folders, reloaded on change; contexts with the same name in
  two files are kept apart as `name@file`.

| Shortcut | Action |
|---|---|
| Ctrl+K | Command palette: resources, pages, contexts, objects |
| Ctrl+Shift+N | New window |
| Ctrl+T / Ctrl+W / Ctrl+Tab | New tab / close tab / next tab |
| Ctrl+B / F5 | Hide the sidebar / refresh |
| Alt+← / Alt+→ | Back / forward (Esc also goes back from a detail page) |
| Ctrl+F or / | Filter the current table: `web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · `/^api-\d+/` · `status:Running` · `restarts>3` · `cpu>200m` (all terms must match) |
| Right-click a row | Open, logs, shell, YAML, events, copy name, favorite |
