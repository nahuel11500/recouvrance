# Features

- **Contexts**: pick one or several on launch; tables merge clusters with a Context column.
- **Live tables** with kubectl's own columns for every type, including CRDs; CPU/memory from
  metrics-server; per-type column choice; multi-namespace filter.
- **Detail pages** for pods (per-container tabs: probes, usage vs requests/limits, ports, env,
  mounts), workloads (scheduling, pods, revisions with diffs), services (port-forward, backing
  pods), secrets (masked, referenced by), ingresses, PVCs, namespaces (quotas), nodes, CronJobs,
  and a generic spec/status view for anything else.
- **History**: an object's page has a History tab listing what changed, newest first, from what
  the cluster already keeps (nothing runs in the background; it is read when the tab is first
  opened): a Deployment's ReplicaSets and a StatefulSet's or DaemonSet's ControllerRevisions
  (what changed in the pod template: images, a restart, the other fields; a rollback says which
  revision it went back to), the revisions of the Helm release that installed the object
  (status, chart, values changed), an Argo CD Application's deployments and a sync that failed
  (and, on an object it deployed, the Application's last deployments), which field managers last
  wrote which fields and when (`managedFields`; status writes left out), and the object's events,
  Warnings highlighted. A pod's page shows its owner's revisions (its ReplicaSet's Deployment).
  *Diff* shows a change's diff, worked out when clicked, the way Compare shows one (the
  pod-template-hash and other noise left out, secret-looking Helm values masked). A Helm
  release's History tab has a *Values diff* against the revision before. A source that can't be
  read (Helm's Secrets forbidden, say) is named; the others still show. ConfigMaps and Secrets
  keep no history in Kubernetes.
- **GPU usage**: utilization and memory per pod, workload, node and cluster, from NVIDIA's DCGM
  exporter (shown when its metrics are in Prometheus or VictoriaMetrics).
- **Usage history** (CPU and memory against requests and limits, over 15 minutes to 7 days; the
  last hour at first) for pods and workloads, from the cluster's Prometheus, found automatically and read through the API server proxy. A workload's charts cover up to 300 of its running or pending pods, and its request and limit lines the same pods; once all its pods have finished (a completed Job), the charts show them without request and limit lines, as they may not have run together.
- **Logs** across all pods of a workload (up to 50, running and newest first), level filter, time range, timeline histogram.
- **Shell** (exec with a real terminal), **node shell** (privileged pod, deleted when its shell
  ends or its page goes away) and **port-forward** for pods and services; forwards are remembered
  and restored with one click. Shells and attach need write mode (they run commands in the
  cluster), checked on every connect, and the context name typed (or a wait) on a protected context.
- **RBAC-aware**: actions your Kubernetes RBAC forbids are disabled; *My access* shows a can-i
  matrix for the namespace, and says when the cluster (webhook or cloud IAM authorizer) may allow
  more than it lists.
- **Problems**: one list of what is broken across clusters (crash loops, pending pods, unavailable workloads, NotReady nodes, failed jobs, pending volumes, and alerts firing in the cluster's Prometheus that nobody silenced).
- **Alerts where you look**: a namespace's page lists the alerts labeled with it, and a pod's, workload's, volume's or node's page the alerts about it (a pod's include its workload's, a workload's its pods'), with severity, summary, how long, runbook link, and silenced or inhibited ones muted when an Alertmanager is found.
- **Stuck deletions**: the Problems page lists objects (pods, workloads, Jobs, PVCs, PVs,
  Namespaces, Argo CD Applications) whose deletion has waited more than 10 minutes, with the
  finalizers holding them (for a Namespace, the resources left); their Delete dialog offers to
  remove the finalizers.
- **ExternalSecrets**: ExternalSecrets that fail to sync and secret stores not Ready show on the
  Problems page; a Secret made by an ExternalSecret shows where it comes from (the ExternalSecret,
  its store, last refresh, Ready), and an ExternalSecret's page links its store and target Secret.
- **Argo CD panel**: a namespace's page shows the Applications deploying into it, and an object's
  page the Application that deployed it (by its tracking id): sync and health, auto-sync,
  revision, last sync, conditions, the last 5 deployments, a link to the Application, and
  *Refresh*/*Sync* (write mode and the hub's protection apply). The Applications are read from the
  hub, found among the connected clusters, else among other kubeconfig contexts that need no
  separate login; when the hub isn't found, name it with `argocd_contexts` (see
  [configuration](configuration.md#argo-cd-hub-argocd_contexts)).
- **Overviews**: cluster capacity and health, workloads status, Helm releases (values masked; install from your repos, roll back and uninstall through your `helm` CLI).
- **API requests** (Settings → API requests): per cluster, how many requests of each kind
  (discovery, lists, watches, metrics, logs, shells, writes) were made, how many failed, and how
  long the API server took to start answering: average, slowest, last.
- **Favorites** and **saved views** (type + namespaces + filter, bookmark button), back/forward history, light and dark themes.
- **Instant back**: the last 8 lists (up to 10,000 rows in all) are kept, so going back to one
  shows it at once, marked "Updating…", while a fresh list loads and replaces it in one go.
- **Bulk actions**: check rows, then restart, scale, delete or copy names. Scaling needs the number
  of objects typed (the replica count is never suggested), deleting asks as set for the kind (see
  below), and each protected context among them needs its name typed (or a wait); rows from several contexts
  show which one they are on.
- **Delete confirmation per kind** (Settings → General → Deleting): delete right away, ask, or
  type the name (the count, for several). By default, what comes back on its own (Pods, Jobs,
  ReplicaSets, Events…) is deleted right away; what takes data, machines or everything inside with
  it (Nodes, Namespaces, StatefulSets, Deployments, DaemonSets, PersistentVolumes and their claims,
  CRDs) needs the name typed; anything else, custom resources included, asks. A kind is added by
  its name, plural or short name (`deploy`), checked against the connected clusters; a custom
  resource is saved with its group (`Cluster.postgresql.cnpg.io`), so a choice for a built-in kind
  never applies to a custom resource of the same name, unless it asks more. Every delete dialog
  links to the setting. A protected context always asks for its name, and an object stuck
  terminating always shows the dialog, to remove its finalizers.
- **Confirming** (Settings → General → Confirming): by default, a confirmation that would ask for
  names typed (a kind set to *Type the name*, a protected context) waits instead: its button stays
  disabled for a few seconds (3 by default, 1 to 30), showing the seconds left (`Delete (3)`),
  then works. A dialog that would ask for several names waits once. Choose *Type the names* to
  type them as before.
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
- **Run as** (Settings → Contexts → *Run as*): set a user (and groups) a
  context's writes may run as, like `kubectl --as`. Write confirmations then show a *Run as
  `<identity>`* checkbox, off each time; checked, that one action runs as the identity and the
  `kubectl` line shows `--as`. A protected context still asks for its confirmation (a wait or its name). Reads go as you; write mode and
  the identity are checked again when each request is sent (an identity changed meanwhile sends
  nothing). Actions disabled because your RBAC forbids them stay enabled when the context has an
  identity. Covered: scale, restart, rollback, delete (force, finalizers), YAML save, the values
  editor, Apply, cordon/uncordon, drain, CronJob run, suspend/resume, Flux reconcile, Argo CD
  refresh/sync, role bindings. Not covered: Helm, bulk delete from tables, plugins, shells, debug,
  node shell, file copy and ServiceAccount kubeconfigs. Agents never run as it.
- **Plugins**: your own commands on resources (`~/.config/recouvrance/plugins.yaml`):
  `$CONTEXT`, `$NAMESPACE`, `$NAME`, `$KIND` are filled in and kubectl points at the cluster; they
  run in a terminal page or in the background. A plugin counts as a write (write mode, protected
  context name typed or a wait) unless it declares `read_only: true`. See Settings → Plugins for an example.
- **Contexts** can get a color (a strip at the top of the window) and be marked *protected*: writes
  there ask you to type the context name, or wait a few seconds (see Confirming; agents follow
  their rules instead). Contexts you have not set are guessed from the context,
  user and cluster names and the server URL (`prod`, `prd`, `production`, `live`, `admin`, `root`),
  so new production contexts are protected too. Set both in Settings → Contexts.
- **Diagnosis** on pods and workloads: why it is broken (crash output and exit code, scheduling,
  image pulls, probes, volumes) and what to try.
- **Rollback** of Deployments, StatefulSets and DaemonSets to any revision, with the diff and a
  server dry run first; **attach** to a container's process; **file copy** in and out of containers
  (both ways it runs commands in the container, so it needs write mode, like a shell);
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
  namespaces (when you may not list them), metrics off, an identity to run as; namespaces can also be added by hand.
- **Kubeconfig**: extra files and watched folders, reloaded on change; contexts with the same name in
  two files are kept apart as `name@file`.

| Shortcut | Action |
|---|---|
| Ctrl+K | Command palette: resources, pages, contexts, objects |
| Ctrl+Shift+N | New window |
| Ctrl+T / Ctrl+W / Ctrl+Tab | New tab / close tab / next tab |
| Ctrl+B / F5 | Hide the sidebar / refresh (in a terminal, F5 goes to the program) |
| Alt+← / Alt+→ | Back / forward (Esc also goes back from a detail page) |
| Ctrl+= / Ctrl+- / Ctrl+0 | Zoom in / zoom out / reset the zoom |
| Ctrl+F or / | Filter the current table: `web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · `/^api-\d+/` · `status:Running` · `restarts>3` · `cpu>200m` (all terms must match) |
| j / k / Enter | Move the selection in a table / open the selected row |
| g then p / o / w / h / i / s | Go to Problems / Cluster overview / Workloads overview / Helm / Assistant / Settings |
| ? | List the keyboard shortcuts |
| Right-click a row | Open, logs, shell, YAML, events, copy name, favorite |

Single keys (?, g, j, k) work when no text field or terminal has focus; `?` lists every shortcut.
