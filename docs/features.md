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
  read (Helm's Secrets forbidden, say) is named; the others still show. What the app recorded
  while it watched the object (see Timeline) is listed too, as *Recorded*.
- **Timeline**: what Kubernetes keeps no history of (ConfigMaps, which keys of a Secret changed,
  Services, Ingresses, HPAs, scale changes, custom resources) is recorded while a cluster is
  connected, from what the app already reads: the lists you open (their watch) and the Problems
  scan (Deployments, StatefulSets, DaemonSets, Nodes, Namespaces, volumes, Argo CD Applications,
  every scan; a kind with more than 1000 objects is scanned from its cells alone, and not
  recorded there). No request is added. A change is recorded when something besides the status
  changed: labels or annotations, or a field manager's write (who, and which fields:
  `managedFields`), or the generation; status-only writes, `resourceVersion` and `managedFields`
  churn, leader-election and other noisy annotations are left out. Creations and deletions are
  recorded too, except of what a controller made (a CronJob's Jobs). Pods, events, ReplicaSets,
  ControllerRevisions, Endpoints, EndpointSlices and Leases are never recorded: they change all the
  time and their history is told elsewhere (a pod's by its owner's revisions and its events). The
  **Timeline** page (sidebar, or `Timeline` in the palette) lists the connected clusters' changes
  newest first with their events interleaved (Warnings only by default, all, or none); filters by
  cluster, and `ns:`, `kind:`, `ctx:` and text in its filter box; a row picked shows its diff,
  worked out then, and opens its object. Lists compare labels and annotations only, so their
  records say which fields changed and who changed them but keep no diff of the spec; kinds named
  in **Settings → Timeline** get a watch of their own on every connected cluster (the one cost,
  opt-in), and their records a diff of the whole object, its status left out. A Secret's values
  are never kept, in memory or on disk: only whether each key changed. An object written all the
  time is recorded at most 10 times an hour (the next record says how many were skipped).
  Records are kept on disk per context, 16 MB and 14 days by default (see
  [configuration](configuration.md#timeline)), so the timeline survives a restart.
- **Reachability**: a tab on Services, Ingresses and HTTPRoutes that follows the path a request
  takes and names the first step that breaks, with the `kubectl` command to see it yourself: the
  Ingress's class and the address its controller gave it (for a route, each Gateway, programmed and
  with an address, and the route accepted on it); each backend Service and its port, by number or
  name; the pods its selector matches and the `targetPort` on their containers; ready endpoints, and
  for each pod that is not ready why (readiness probe, pending, terminating), with a link to it; the
  TLS Secret's certificate (there, valid, covering the hosts). Everything is read in the object's own
  cluster; a read that fails is shown as not checked rather than as broken. *Test from this machine*
  resolves each host of an Ingress and sends it a request (status code, TLS error, time), and says
  when the name doesn't resolve to the Ingress's address. NetworkPolicies are not predicted. The same
  checks are the agents' `check_reachability` tool.
- **GPU usage**: utilization and memory per pod, workload, node and cluster, from NVIDIA's DCGM
  exporter (shown when its metrics are in Prometheus or VictoriaMetrics).
- **Usage history** (CPU and memory against requests and limits, over 15 minutes to 7 days; the
  last hour at first) for pods and workloads, from the cluster's Prometheus, found automatically and read through the API server proxy. A workload's charts cover up to 300 of its running or pending pods, and its request and limit lines the same pods; once all its pods have finished (a completed Job), the charts show them without request and limit lines, as they may not have run together.
- **Logs** across all pods of a workload (up to 50, running and newest first), level filter, time range, timeline histogram.
- **Shell** (exec with a real terminal), **node shell** (privileged pod, deleted when its shell
  ends or its page goes away) and **port-forward** for pods and services; forwards are remembered
  and restored with one click. Shells and attach need write mode (they run commands in the
  cluster), checked on every connect, and the context name typed (or a wait) on a protected context.
- **Files** on a pod: a container's filesystem as a folder tree, read through exec with the tools
  it has (`sh`, `stat`, `head`, `find`, GNU or busybox). Open folders (size, mode, owner,
  modification time, symlink targets), preview a text file (its first 256 KiB; a binary file shows
  its type), download a file or a folder (as a `.tar`) to ~/Downloads, search by name under the
  folder shown, or type a `/path` to go there. Each container keeps its own place. Names with
  spaces, newlines or bytes that aren't UTF-8 are shown escaped and still open. Like a shell it
  needs write mode, and the context name typed (or a wait) once on a protected context; it is
  greyed out when your RBAC does not allow `pods/exec`. A container without a shell or those
  tools (distroless, scratch) is offered a **debug container**, like `kubectl debug --target`:
  one confirmation shows the `kubectl debug` equivalent and that an ephemeral container stays in
  the pod's spec until the pod is replaced. It runs the node shell image (busybox by default),
  shares the container's processes, runs as its user (from what the kubelet reports, else the
  container's or the pod's `securityContext`) and sleeps for 4 hours; the files are read live
  through `/proc/<pid>/root` of its main process, symlinks followed as the container sees them,
  and the toolbar says *through a debug container*. It uses `kubectl debug`'s `general` profile
  (`SYS_PTRACE`), none added where Pod Security is `baseline`, and `restricted`'s where it
  requires it. Browsing again reuses one still running; it is greyed out when your RBAC does not
  allow `patch` on `pods/ephemeralcontainers`.
  A container that isn't running (crash loop, exited, waiting) is read from a **copy of the pod**,
  after one confirmation that shows the `kubectl debug <pod> --copy-to=… --same-node` equivalent:
  the same spec and volumes (mounted ConfigMaps and Secrets are there) on the same node, without
  its labels, annotations, owner references, init containers and probes (no Service sends it
  traffic, no controller adopts it), every container sleeping instead of starting its app. A
  distroless image has no `sleep`: an init container copies a static busybox (`busybox:1.37-musl`,
  or the cluster's node shell image when set) into the copy, at `/.recouvrance-copy`, and the copy
  runs and is read with it. The copy shows *copy of the pod* next to its files, and says that files
  the container wrote while it ran are not there (it starts from the image again). It is deleted,
  by its uid, when the page closes, on its *Delete the copy* button and when the app quits; it
  stops by itself after an hour, and one left behind (the app killed) is deleted the next time the
  app connects to its cluster (pods labelled `recouvrance/pod-copy` older than an hour). It needs
  write mode and RBAC to create and delete pods in the namespace (greyed out with the reason
  otherwise). Agents' `browse_files` reads running containers only.
- **RBAC-aware**: actions your Kubernetes RBAC forbids are disabled; *My access* shows a can-i
  matrix for the namespace, and says when the cluster (webhook or cloud IAM authorizer) may allow
  more than it lists.
- **Problems**: one list of what is broken across clusters (crash loops, pending pods, unavailable workloads, NotReady nodes, failed jobs, pending volumes, and alerts firing in the cluster's Prometheus that nobody silenced).
- **Alerts where you look**: a namespace's page lists the alerts labeled with it, and a pod's, workload's, volume's or node's page the alerts about it (a pod's include its workload's, a workload's its pods'), with severity, summary, how long, runbook link, and silenced or inhibited ones muted when an Alertmanager is found.
- **Upgrade check** (Cluster overview → *Upgrade check*): per cluster, what needs action before it
  moves to a newer minor version (the next one by default; the arrows pick another), most urgent first:
  - *Removed APIs still used*: objects last written in an API version the target no longer serves
    (by which field manager, or by `kubectl apply`'s last-applied configuration), Helm releases whose
    last manifest names one (even a version already removed: Helm can't upgrade them), and what the API
    server's `apiserver_requested_deprecated_apis` metric says clients still call. Each says what to
    migrate to (e.g. HorizontalPodAutoscaler `autoscaling/v2beta2` → `autoscaling/v2`) and what
    changes on the way. The removals are those of the Kubernetes deprecation guide.
  - *Version skew*: kubelets and kube-proxies (by their image in `kube-system`) older than the skew
    policy allows for the target, or newer than it, and a target more than one minor version ahead
    (the control plane goes up one at a time).
  - *Drains that would block*: PodDisruptionBudgets allowing no disruption now (one guarding a single
    pod waits forever) and pods several budgets select (the eviction API refuses them).
  - *Feature gates*: the API server's deprecated gates, and alpha ones turned on (from its
    `kubernetes_feature_enabled` metric; command-line flags can't be seen from the API).
  - *Add-ons*: CRDs serving versions marked deprecated, and whether objects may still be stored in
    them.

  Each finding links to its objects. A part that can't be read (the API server's metrics need `get`
  on `/metrics`) is named, and the rest is still checked.
- **References to missing objects**: the Problems page lists what points to something that
  doesn't exist, on the object that points to it: a Service that selects no pod (minor when its
  Deployment or StatefulSet is scaled to 0) or no ready one, an Ingress whose backend Service,
  its port or a TLS Secret is missing, an HTTPRoute whose backend Service is missing (when the
  cluster serves the Gateway API). A pod stuck creating, a workload short of ready replicas or a
  pending PVC says which ConfigMap, Secret, PVC, ServiceAccount or StorageClass it waits on
  (`optional: true` references aren't needed, and a running pod needs none any more). Kinds that
  can't be read are named, not reported missing.
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
- **Argo CD page** (sidebar → *Argo CD*): every Application of every context that serves Argo
  CD, read with one list per Argo CD cluster: name, project, sync, health, revision, destination
  (the kubeconfig context whose API server is the Application's `destination.server`, and its
  namespace), auto-sync, last sync and its result. Applications sit under the app of apps that
  made them (its tracking id, the parent listing them among its resources, or the instance
  label), with how many they hold and how many of those are red; groups fold. Filters: out of
  sync, Degraded, failed sync (any of those picked), project, destination context, and a search.
  Red is for a failed sync, an error Argo CD reports or Degraded health: an Application with
  auto-sync off that is out of sync (a `*-workload` one synced by hand) is not red. A click opens
  the Application's page, which links to its destination namespace (connecting to that context
  when it isn't); the row menu also opens the destination and asks for *Refresh*, *Hard refresh*
  or *Sync* (write mode and the Argo CD cluster's protection apply). An Argo CD cluster that
  can't be read is named above the list; the others are shown.
- **Argo CD diagnosis**: an Application's page says what is wrong with it, worst first, each
  with its objects (opening on the destination's context when it is connected) and what to do:
  - *Being deleted*: the finalizers holding it (what each one waits for) and the resources not
    deleted yet; red after 5 minutes.
  - *Sync running* for more than 10 minutes: the hook or resource it waits for.
  - *Last sync failed*: each object that failed and why, from Argo CD's messages: a hook that
    failed, rejected by admission (a webhook, Pod Security), a webhook that did not answer, a
    kind not served (CRD missing), a namespace that does not exist, a quota, RBAC, the
    AppProject, an immutable field, an invalid manifest. Errors Argo CD reports
    (`ComparisonError`...) too.
  - *Sync loop*: the same revision synced 3 times or more within an hour by auto-sync (not by
    a user), the resources it applies again each time, and the `ignoreDifferences` (with
    `RespectIgnoreDifferences=true`) that would stop it, made from the fields that differ.
  - *Out of sync*: each resource that differs, is missing or would be pruned, and whether git
    moved since the last sync. With auto-sync off (a `*-workload` Application) it is said, not
    as a problem.

  For each out-of-sync resource, the fields that differ are shown as a diff, read on the
  connected context that reaches the destination. Git's manifests can't be read through the
  Kubernetes API (Argo CD serves its own diff behind its own login), so the live object is
  compared with what was last applied to it (the `last-applied-configuration` Argo CD's sync
  writes): defaults the API server adds are left out, quantities written another way (`1000m`
  and `1`) are the same, items added to a list (an injected sidecar) are shown, and a
  Secret's values never are. The field managers that wrote the object since the last sync are
  named too (with server-side apply that is all there is). A destination that isn't connected
  is said; open it to connect.
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
  *Refresh*/*Hard refresh*/*Sync*, create/apply YAML (a server dry run first, then a confirmation listing each
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
