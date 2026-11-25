# Features

- **Contexts**: pick one or several on launch; tables merge clusters with a Context column. Each
  context connects on its own: the first rows show once one cluster has answered and the others
  have had 2 more seconds, so a cluster that can't be reached, or a login waiting in the browser,
  doesn't hold back the others' rows. Until one of the set answers, the list's place shows each
  context as "Connecting…" or "Can't connect: <why>" (never an empty "No resources"), with Retry
  and Choose contexts; once others show rows, those still connecting or failed get a line each
  above the list. The context picker and the Contexts page mark them too ("connecting…", "can't
  connect" with the reason in a tooltip). A cluster still connecting joins the list when it
  answers, and the page on screen then (a Cluster or Argo CD overview is built again). Retry
  connects to the whole set again. Once every cluster is in, the set's session is saved again
  (and reopened whole if it was reopened without them and nothing changed since). The Pods list starts with the connect,
  alongside the cluster's discovery and namespace list, when it is the list shown first. When a
  context's login command (its kubeconfig exec plugin) fails, the app says which program failed and
  its exit code: never its arguments, environment or output, where a client secret or a token can
  be. It adds the last line the plugin wrote to stderr when the kubeconfig sets
  `interactiveMode: Never`; otherwise the plugin writes to the terminal Recouvrance was started
  from. A plugin that cannot start, often because it is not on the `PATH` the app was started
  with, is reported with its name. A proxy the app can't use (the kubeconfig's `proxy-url`, or
  `HTTPS_PROXY`, with a scheme other than `http`, `https` or `socks5`, or none) is named by its
  scheme, host and port, never its user and password, even a password with an unescaped `/`, `?`
  or `#`; by its scheme alone when its text holds a `#`, or when the host can't be told from the
  password. Neither shows for any other URL in a connection error, unless its password holds an
  unescaped `/`, `?` or `#` (#1000).
- **Context sets**: tick contexts in the context picker, then *Save as set…* and name them (for
  example "prod" for every production region). The sets come first in the picker, on the
  Contexts page and in the palette (Ctrl+K, type the set's name or `set`); one click connects
  all of a set's contexts, in the order saved (the primary context then stays the primary). A context no longer
  in the kubeconfig is left out ("2 of 3 contexts"), and a set with a protected context shows
  the lock. Saving under a name already taken replaces that set; the × on a set's row forgets
  it. A set reopens where it was left, like any set of contexts connected together (`sessions`).
- **Short context names**: a context named after its cloud cluster shows the cluster's name
  (`arn:aws:eks:<region>:<account>:cluster/<name>` and `gke_<project>_<zone>_<name>` show
  `<name>`) in the top bar, the sidebar, the context picker, the Contexts page, the connecting
  and can't-connect lines, the Problems page and the Context column. When two would read the
  same, the region is added, then the account or project; a short name never reads as another
  context's name. The full name shows on hover. The picker's search finds either; the Problems
  filter matches the full name, which holds the short one. The Context column sorts by the
  short name, and its `context:` filter term compares the full name. Settings, protection, agent
  rules, kubectl lines, copied text and the agents' answers keep the full name.
- **Reopens where you left off**: each set of contexts keeps its tabs (the list or overview each
  shows, its resource type, namespaces and filter) and each type's filter, and the window keeps
  its place, size and whether it was maximized. On launch the first window reopens there once its
  contexts connect: with one namespace picked last time, a big cluster lists and watches only that
  namespace at launch instead of every pod. A resource type, namespace or page the clusters no longer have falls back to the default; a
  detail page reopens on the list or overview it was opened from; a window that would be out of
  reach (a display unplugged) is centered. A window opened later starts fresh, and write mode is
  always off at launch. While a cluster of the set doesn't answer, or its namespaces don't list
  (said above the list), or joins after the first rows, nothing is saved for that set: what only
  that cluster has would read as gone; connecting that set again once every cluster answers reopens it from what was saved.
  An add-on API that doesn't answer (an APIService down, such as Argo CD's or metrics-server's)
  is named above the list (with its cluster when several are connected), and its types may be
  missing from the sidebar or out of date; a tab or filter on one of its types, saved or left open
  when connecting that set again, shows the default list meanwhile, but is saved as it was until
  you change that tab.
  A set connected for the first time is saved once connected. With two windows on one set, the
  one that reopened it saves it, until something is changed in the other.
- **Sidebar**: the filter at its top finds pages and resource types (by name, kind or API group).
  Pages, saved views, favorites and types scroll together; a section folds with a click on its
  name (kept for next time) and says how many types it holds. Port forwards fold into one row at
  the bottom. Drag its edge to resize it: the width is kept, and a cut name shows whole in a tooltip.
  It leaves out events.k8s.io's copy of Events, the old Endpoints and API priority and fairness
  (FlowSchemas); Apply, links and the agents still know them.
- **Live tables** with kubectl's own columns for every type, including CRDs; CPU/memory from
  metrics-server, as bars against each pod's requests and limits (listed once, then kept by a
  watch: new and resized pods show without listing every pod again); per-type column choice;
  multi-namespace filter; each type keeps its own filter text (Nodes doesn't show the pod name
  typed on Pods, and Pods has it back).
- **Usage timed to metrics-server**: metrics-server measures every `--metric-resolution` (15 s by
  default, often 60 s), so reading it more often returns the same numbers. The pods and nodes
  lists learn the interval from the samples' timestamps and read again just after the next one
  is due (never more often than `metrics_refresh`, never later than twice the interval or
  5 minutes; every `metrics_refresh` when the timestamps don't move or don't add up). The CPU and
  Memory headers' tooltip, and the Containers and Utilization headings of pod and workload pages,
  say how old the numbers are: "as of 40s ago · metrics-server measures every 60s". Ages are
  counted on the API server's clock (its `Date` header), so a skewed local clock doesn't matter.
- **Node status**: a node's Status adds, after kubectl's own words, the taints that keep pods off
  it because something is wrong, kubectl-style (`Ready,CiliumAgentNotReady`): the
  `node.kubernetes.io/*` ones (pressure, network unavailable, out of service), the cloud
  provider's (`Uninitialized`, `Shutdown`), Cilium's agent not ready, the cluster autoscaler's
  and Karpenter's (`ToBeDeleted`, `DeletionCandidate`, `Disrupted`, `Unregistered`), and others
  whose name says not ready, unavailable, uninitialized, unreachable or pressure. The taints set
  on purpose to dedicate nodes (`production=true`, GPUs, control plane, `node-role.*`) don't
  show. Such a node, or a cordoned one (`SchedulingDisabled`), is yellow; NotReady and Unknown
  red. `status:` filters and sorting see the labels; the node's page and the cluster overview
  show them too. The taints are in a node's `spec`: the Nodes list reads whole nodes once, then
  watches their metadata (a whole node is mostly its image list, and its status is rewritten every
  few seconds) and reads one node again when its `managedFields` show a new write to its spec.
- **Detail pages** for pods (per-container tabs, native sidecars, init and debug containers
  included: probes, usage vs requests/limits, ports, env, mounts), workloads (scheduling, pods, revisions with diffs), services (port-forward, backing
  pods), secrets (masked, referenced by), ingresses, PVCs, namespaces (quotas), nodes, CronJobs,
  and a generic spec/status view for anything else. A page follows its object and its pods with
  watches: a change shows about a second after it happens, nothing is read again while nothing
  changes, and an object deleted then created again under its name is followed.
- **Map**: every detail page has a Map tab, what the object is linked to. A pod or workload
  starts with its tree (Services, the ConfigMaps, Secrets, PVCs and ServiceAccount its pods use,
  not the `kube-api-access-*` volume every pod gets; ReplicaSets, pods, nodes). Then, for every
  kind: its owners and what it owns; the pods a Service, PDB or NetworkPolicy selects, and the
  PDBs, NetworkPolicies, HPA and Ingresses or HTTPRoutes (through its Services) of a pod or
  workload; what uses a Secret or ConfigMap (pods and workloads by env, envFrom, volumes and pull
  Secrets, a ServiceAccount, an Ingress's TLS) and where it comes from (an ExternalSecret, a
  cert-manager Certificate); a ServiceAccount's Secrets, pods and workloads; a PVC's volume,
  StorageClass and pods; an Ingress's or HTTPRoute's Services with the pods behind them, TLS
  Secrets, class or Gateways; an HPA's or VPA's target; a binding's role and ServiceAccounts; a
  node's pods. An operator's custom resource also shows the operator's other resources naming it
  in their spec (a CloudNativePG Cluster's Backups, ScheduledBackups, Poolers and Databases by
  known rules; a generic rule for `cluster`, `clusterName` and `…Ref` fields, where the field
  says the kind: `clusterRef` a `…Cluster`, `issuerRef` an Issuer, a written `kind` first) and,
  last, what is labelled with its name (`cnpg.io/cluster` and other operators' labels;
  `app.kubernetes.io/instance` unless Helm manages the object, where it names the release).
  Kinds with no rule of their own (a Gateway, a StorageClass, a ClusterRole) say that only
  owners are known. It is read when the tab opens, in the object's namespace only, from the API
  server's cache (metadata alone where that is enough; an operator's kind with more than 200
  objects is not read whole, and the tab says so), next to what the page already holds; a
  cluster-scoped object reads nothing more. The relations are worked out off the UI thread.
  Each relation, and the pods behind each Service, shows 50 objects and counts the rest. What
  could not be read (by the page or the tab) is named, and the tab says "nothing found" only
  once everything was read.
- **Select and copy text** on detail pages, as on a web page: a drag selects (across fields,
  labels, annotations, conditions, events, the History, Compare and Reachability tabs), Ctrl+C (⌘C
  on macOS) or a right-click *Copy* copies it, a double-click selects a word. Text cut short with
  `…` is selected and copied whole. Links still open on a click; a drag over one selects its text.
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
  recorded too (a creation shows after 30 s), except of what a controller made (a CronJob's Jobs,
  or a Secret an operator gives itself as owner just after making it); a change to the owner
  references alone is not recorded. Pods, events, ReplicaSets,
  ControllerRevisions, Endpoints, EndpointSlices and Leases are never recorded: they change all the
  time and their history is told elsewhere (a pod's by its owner's revisions and its events). The
  **Timeline** page (sidebar, or `Timeline` in the palette) lists the connected clusters' changes
  newest first with their events interleaved (Warnings only by default, all, or none; those the
  cluster still keeps, an hour's worth by default, read once and then kept current by a watch, so
  a refresh brings only what changed (read again at each overview refresh where they may be
  listed but not watched); the newest 20,000 of a cluster are held, shared by its
  namespaces when they are read one by one, and the page says how many it shows of how many and
  how far back they go when the cluster keeps more; the events of one object with the same
  reason make one row, with their count); shows the namespaces
  picked in the namespace picker only (cluster-scoped objects such as Nodes too when none is
  picked), and filters by cluster, and `ns:`, `kind:`, `ctx:` and text in its filter box; a row picked shows its diff,
  worked out then, and opens its object. Lists compare labels and annotations only, so their
  records say which fields changed and who changed them but keep no diff of the spec; kinds named
  in **Settings → Timeline** get a watch of their own on every connected cluster (the one cost,
  opt-in), and their records a diff of the whole object, its status left out. A Secret's values
  are never kept, in memory or on disk: only whether each key changed. An object written all the
  time is recorded at most 10 times an hour (the next record says how many were skipped).
  Records are kept on disk per context, 16 MB and 14 days by default (see
  [configuration](configuration.md#timeline)), so the timeline survives a restart. Agents read
  it, with its events, through `recent_changes`, under their rules (see [agents](agents.md)).
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
- **Logs of the checked rows**: check pods, workloads (Deployments, StatefulSets, DaemonSets,
  ReplicaSets, Jobs) or Services in any table, merged multi-cluster ones included, then **Logs** in the
  selection bar. One page merges their pods' lines in time order, each tagged with its pod and
  container (and its namespace or context when they differ), with the same filters, find, Since,
  Follow and container picker. It reads at most 50 rows and follows at most 50 pods (running and
  newest first), and says how many were left out. Each cluster's pods show as soon as its rows are
  read: a cluster that fails, or does not answer within 15 seconds, is named and never holds back the
  others' pods. F5 finds their pods again.
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
- **Problems**: one list of what is broken across clusters (crash loops, pending pods, unavailable workloads, NotReady nodes, nodes such a taint keeps pods off for more than 5 minutes after they started, failed jobs, CronJobs that stopped running (the second run their schedule and time zone call for since the last one hasn't started, `startingDeadlineSeconds` added) or whose last 3 runs or more didn't succeed (suspended ones are left out), pending volumes, certificates that fail to renew or end soon (see Certificates below), Argo CD Applications that need attention, and alerts firing in the cluster's Prometheus that nobody silenced). Click a count (critical, warnings, minor) to show that severity alone; the filter box takes text and `ns:`, `kind:` (`kind:alert`, `kind:service`) and `ctx:` terms, as the Timeline's. One scan per cluster serves the page, the sidebar count of every window and the agents' `find_problems`: a cluster is scanned again every 2 minutes (eight times as long as a scan takes, when that is longer), a hidden window starts no scan, though an agent's `find_problems` still scans when the latest findings are too old. The page opens on the latest findings at once and says how old they are; during a scan each check's new findings show as soon as it finishes, and Refresh asks for a scan started after it (a scan already running is waited for, then another one runs). A Service with no ready endpoint is looked into once, then again when it changes, after 10 minutes, or (when it selects no pod because a workload is scaled to 0) once that workload is scaled up. The first count waits for the list on screen. A scan started by the sidebar reads at most 8 of the 32 namespaces read at once, so the page on screen always has the others; once the page, Refresh or an agent waits for it, it reads in the foreground. A page scoped to some namespaces shows them from the sidebar's scan of the cluster when there is one; Refresh there scans only those namespaces. Refresh and an agent's `max_age_seconds: 0` judge every Service again. The sidebar count's tooltip says how old it is, and how often it comes.
- **Alerts where you look**: a namespace's page lists the alerts labeled with it, and a pod's, workload's, volume's or node's page the alerts about it (a pod's include its workload's, a workload's its pods'), with severity, summary, how long, runbook link, and silenced or inhibited ones muted when an Alertmanager is found. A Service's, HorizontalPodAutoscaler's or Ingress's page shows those labeled with it (`TargetDown`, `KubeHpaMaxedOut`; a `service` label counts only on a scrape target's alert, with its `job`, since rules often set one to route alerts; an Ingress is looked for in `exported_namespace` when the alert has it, as ingress-nginx's do), and a node's page those whose `instance` is the node's name or address (node-exporter's). **Rule** on an alert shows the rule it comes from, read from Prometheus' rules API on that click: its expression, how long it must hold before firing, its labels and annotations, an error when its last evaluation failed, and a chart of the last 3 hours for that alert's series alone (matched on the alert's labels, without the rule's and vmalert's `alertgroup`). The chart draws the comparison's left side, with the threshold as a line, since the comparison itself drops every point under it.
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
  - *Feature gates*: deprecated gates the API server's `--feature-gates` sets, and alpha ones turned
    on (from its `kubernetes_feature_enabled` metric). The flags are read from its pod in
    `kube-system` (kubeadm, kind); where it has none (a managed control plane), every deprecated
    gate is listed to look for in them.
  - *Add-ons*: CRDs serving versions marked deprecated, one finding per set of versions, and whether
    objects may still be stored in them.

  Each finding links to its objects. A part that can't be read (the API server's metrics need `get`
  on `/metrics`) is named, and the rest is still checked.
- **References to missing objects**: the Problems page lists what points to something that
  doesn't exist, on the object that points to it: a Service that selects no pod (minor when its
  Deployment or StatefulSet is scaled to 0, or when it picks pods of a running one by a role
  label the operator sets on them and no pod has that role now while every pod has another, as
  a Postgres replica Service with one instance) or no ready one, an Ingress whose backend Service,
  its port or a TLS Secret is missing, an HTTPRoute whose backend Service is missing (when the
  cluster serves the Gateway API). A pod stuck creating, a workload short of ready replicas or a
  pending PVC says which ConfigMap, Secret, PVC, ServiceAccount or StorageClass it waits on
  (`optional: true` references aren't needed, and a running pod needs none any more). Kinds that
  can't be read are named, not reported missing.
- **Volumes filling up**: a PVC's page shows its used and free space and its inodes, as the
  kubelet of a node running a pod that mounts it measures them (read for the page shown; it needs
  `nodes/proxy`, and a PVC no running pod mounts says "not mounted"). The Problems page lists the
  PVCs over 90% of their space or inodes (critical from 97%), from one query to the cluster's
  Prometheus per scan (`kubelet_volume_stats_*`), nothing more asked of the API server; a PVC its
  own firing alert (such as `KubePersistentVolumeFillingUp`) already names, or one whose alert is
  silenced, isn't listed twice. Without Prometheus there is nothing to list; one that doesn't
  answer is named as not checked.
- **Stuck deletions**: the Problems page lists objects (pods, workloads, Jobs, PVCs, PVs,
  Namespaces, Argo CD Applications) whose deletion has waited more than 10 minutes, with the
  finalizers holding them (for a Namespace, the resources left); their Delete dialog offers to
  remove the finalizers.
- **Argo CD Applications that need attention**: the Problems page lists, under the cluster and
  namespace they deploy to, the Applications that are Degraded (critical), whose last sync failed,
  that report an error condition (`ComparisonError`, `SyncError`, `InvalidSpecError`...), whose
  sync has run for more than 10 minutes, or that have resources Missing with auto-sync on
  (warnings). An Application synced by hand is never listed for being out of sync, nor for what it
  has not deployed yet (Missing). They count in the sidebar badge and `find_problems` reports them.
  They are read from Argo CD's API (see *Argo CD by its API*): every server the argocd CLI is
  logged in to or set in Settings → Argo CD, the feeds the Argo CD page holds while it is open
  (no request then), else one list per server at most once a minute for every cluster scanned. A
  server that doesn't answer within 20 seconds, or can't be logged in to, is named as not checked,
  and that answer serves every cluster's scan for the minute. An Application goes under the
  cluster its destination maps to in the kubeconfig (by the API server's address, or by name; its
  `in-cluster` is the cluster whose Argo CD server is that one in Settings → Per cluster), the
  reason names its Argo CD, and a row opens its dashboard on the Argo CD page. An agent sees one
  when its rules let it `list` `applications.argoproj.io` with the server's name as the context.
  With no Argo CD server logged in, they are read on their hub instead, which needn't be
  connected (a row opens the Application's page there; the reason names the hub when it is
  another cluster): the cluster scanned when it serves Argo CD, and those found this session by
  the Argo CD page or an object's Argo CD section (only the ones set in Settings, when some are).
  A scan in the background asks no other context, and so never starts a login for a context not
  logged in this session. Each hub's Applications are read lean, from the API server's cache, at
  most once a minute for every cluster scanned, with the same 20 seconds; a cluster whose
  Applications you may not list (access to some namespaces only) shows no Argo CD line.
- **ExternalSecrets**: ExternalSecrets that fail to sync and secret stores not Ready show on the
  Problems page; a Secret made by an ExternalSecret shows where it comes from (the ExternalSecret,
  its store, last refresh, Ready), and an ExternalSecret's page links its store and target Secret.
- **Certificates**: the Problems page shows cert-manager Certificates that expired, that have not
  been Ready for 10 minutes, or whose renewal is more than an hour late (from their own
  `renewalTime`, so short-lived certificates are judged fairly), critical with less than 3 days
  left; and `kubernetes.io/tls` Secrets whose certificate expired (critical, a warning once it
  expired more than 30 days ago: most likely a leftover), is not valid yet, or ends within 14
  days and a third of its lifetime. A Secret whose `cert-manager.io/certificate-name` Certificate
  exists in its namespace is judged by that Certificate; one whose Certificate was deleted (or
  that was copied elsewhere) is judged itself, since nothing renews it. The Secrets are listed
  from the API server's cache with only `tls.crt` read (the keys are skipped while parsing),
  with the names of the Certificates beside them, at most every 10 minutes per cluster and
  namespace scanned; Refresh and an agent's `max_age_seconds: 0` read them again. A user who may
  not list Secrets only gets the Certificates, and the refused list is tried again at most every
  10 minutes too. A Certificate's page shows its Secret, issuer, validity, renewal
  time, failed attempts, why it fails, and its last CertificateRequest (failed, issued or
  pending, and why).
- **Argo CD panel**: a namespace's page shows the Applications deploying into it, and an object's
  page the Application that deployed it (by its tracking id): sync and health, auto-sync,
  revision, last sync, conditions, the last 5 deployments (each but the newest with *Roll
  back…*), a link to the Application, and *Refresh*/*Sync* (*Terminate* while an operation runs;
  write mode and the server's or hub's protection apply). The Applications are found in what
  the Argo CD API servers' feeds hold (those the argocd CLI is logged in to and those added in
  Settings → Argo CD): one the Argo CD page follows already is read as it is, else the page
  starts it and it stays 5 minutes after the last page that asked; a server that is down is
  said, never waited on again meanwhile. An object's Application is read whole once found, for
  the object's own state in it; a namespace's are those whose destination is that namespace
  of this cluster. One read by the API opens on the Argo CD page's dashboard, and its actions
  go through Argo CD's API (a sync of the whole Application). Only when no server has it are
  they read from the hub, found among the connected clusters, else among other kubeconfig contexts that
  need no separate login. The hub can be ticked in Settings → Contexts (*Argo CD*; see
  [configuration](configuration.md#argo-cd-hub-argocd_contexts)).
- **Argo CD page** (sidebar → *Argo CD*): the environments of every Argo CD API server the
  argocd CLI is logged in to or set in Settings → Argo CD, read only through Argo CD's API (no
  kubeconfig for Argo CD's cluster needed, and none is asked: see
  [configuration](configuration.md#argo-cd-by-its-api-argocd_api_servers)). With no server, the
  page says how to add one. It opens on an *Overview* of the environments the filters keep
  (*Environments* shows the list; which one is remembered): counts per environment (synced and
  healthy, out of sync, Degraded, sync failed, syncing; a click shows them in the list), each
  type with how many need a look, the environments that need a look (a failed sync, Degraded, a
  sync running for over 10 minutes, out of sync with auto-sync on), production first; per
  project, the version each type runs of the product's images (the tag most of its
  environments run, `prod 1.4.2 · preprod 1.5.0-rc.1 · staging main-77b324b`) and the
  environments on another tag, named (components on the same tags everywhere on one line); and
  the last operation of each environment (when, the Application, the
  result, who started it, the revision). Review environments of other people (the type
  `review` whose name doesn't hold the login of the Argo CD token as a word) are left out
  unless asked for. An *environment* is a root Application (an app of apps) and the
  Applications it made (its tracking id, the parent listing them among its resources, or the
  instance label), or an Application on its own; above an app of apps, a *bootstrap* that made
  apps of apps is an environment of its own, and its apps of apps each one. One row per
  environment, grouped by project, then by type in the order Settings → Argo CD gives them (by
  default prod, preprod, acc on a tag, acc, staging, review, platform, then other: from the
  root's name, see [configuration](configuration.md)), or by project only, or not grouped; each
  heading says how many environments it holds and how many need a look, and folds. A row: the
  worst color of its Applications, its name and how many Applications, what is wrong as badges
  (*Sync failed*, *Degraded*, *OutOfSync*, *Syncing*), the images it runs with their tag (a
  commit sha cut to 7: `main-77b324b`), the product's first (the images whose tag differs between
  the project's environments; a database or exporter every environment runs alike comes after,
  all of them on hover), when its last sync ended, and where it deploys (Argo CD's name of the
  cluster, from the Application or Argo CD's cluster list, and the namespace) when that differs
  between rows: a column with the same value on 95% of the rows is left out, and the Argo CD
  server's column shows only with several. Its chevron unfolds its Applications (named after the
  environment: `workload`, `managed-namespace`), each with its sync, health, revision, auto-sync,
  the next time it can sync and its last sync. *Next sync* reads its project's sync windows
  (`allow` and `deny`, their cron schedule, duration and time zone, the windows applying by name,
  namespace or cluster): *no window*, *now*, *in 5h*, or *not within 8 days*; auto-sync is held
  by every window that applies, a manual sync goes through those with `manualSync`. Filters:
  *Needs a look* (out of sync, Degraded or a failed sync), *Out of sync*, *Degraded or failed*,
  *Running* (any of those picked), favorites, project, type (or *On a version tag*: the
  product's images all on a version tag such as `1.4.2` or `26-8-4-<sha>`, not a branch build),
  and a search, each word in turn, in the names, namespaces, clusters, project, server and image
  tags (find every environment on a tag), where `key=value` is a label of one of its
  Applications (`key=` any value of it). The star on a row makes the Application a favorite;
  environments with one come first in their group. The filters, the search, the grouping, the
  folded headings and the unfolded environments are remembered (settings.json's `argocd_page`)
  and the page opens on them again, after a restart too. The groups are worked out once per change of
  the feeds, off the UI thread, and only the rows on screen are drawn.
  Red is for a failed sync, an error Argo CD reports or Degraded health: an Application with
  auto-sync off that is out of sync (a `*-workload` one synced by hand) is not red. A click opens
  the environment's dashboard (below); a row's menu opens the Application (an environment's:
  its root) in Argo CD's web UI or its destination namespace, picks the environment's
  Applications, and asks for *Refresh*, *Hard refresh*, *Sync*, *Terminate* (while an operation
  runs) or *Roll back to the previous deployment* (auto-sync off) (see *Argo CD by its API*). An
  Argo CD server that can't be read is named above the list; the others are shown.
- **Argo CD by its API**: Applications are read from Argo CD's own API, with the argocd CLI's
  login (`argocd login`, its contexts, servers and tokens; `--insecure`, `--plaintext` to this
  machine and `--grpc-web-root-path` are honoured, `--core` contexts have no server) or a token
  set in Settings → Argo CD. Argo CD's application stream is opened first, then one list (`GET
  /api/v1/applications`, only the fields the page shows), what the stream sent meanwhile applied
  over it: no change is lost between the two. The stream keeps them live, opened again with a
  backoff (1 s doubling to 60 s) when it ends: nothing is polled, and nothing is read while no
  window shows the page. The sync windows come from one list of the projects, read again every 5
  minutes (or each project, when they can't be listed). *Next sync* is worked out once a minute,
  once for each set of windows.
  A row's menu opens the Application in Argo CD's web UI; its *Refresh*, *Hard refresh*,
  *Sync* (of the whole Application, with its own sync options), *Terminate* and *Roll back* go
  through the same API, need write mode when they are sent, and are confirmed like a protected
  context's (the server's name typed, or a wait). Settings → Argo CD lists the servers, says how
  each logs in, tests one (who the token is, Argo CD's version, how many Applications it may
  read), turns one off, and never shows a token. **Connect to Argo CD…** (palette, Settings →
  Argo CD, the Argo CD page) takes the address as pasted, reads how the server signs in and
  offers only those ways: SSO in the browser (no CLI needed; kept signed in with its own refresh
  token), a username and password, or a token; *Reconnect* shows where a login expired or was
  refused (see [configuration](configuration.md#argo-cd-by-its-api-argocd_api_servers)).
- **Argo CD environment dashboard**: a row's click opens the dashboard of its environment,
  titled with its name, its type and its Argo CD; *‹ 3 / 12 ›* goes to the environment before or
  after it among those the list's filter keeps, in the list's order. An entry per Application of
  the environment, its workload (the one running the most images) first, its root (the app of
  apps, whose cards list its children) last, each named under the environment (`workload`,
  `managed-namespace`) with a dot in its color, its sync and health (with its Argo CD's name too
  when two share a name or several Argo CDs are read); a click shows its cards, the workload's
  first. *Updated at* and *Refresh*, and cards. *State*: the sync in big (*Synced*, *OutOfSync*, *Sync Failed* or
  *Syncing*), the health, the resources Degraded (`worker Degraded +1`) and *Why?* (the diagnosis
  below), the card tinted green, amber or red. *Versions*: a chip per component's image (the
  repository's last part and its tag), red with `1.4.1 → 1.5.0` when it changed since the
  deployment before (from the images seen running under it, else those its sources pinned:
  kustomize images, Helm `*repository`/`*tag` parameters), who released it and when (`autosync`
  for auto-sync), what ran before, and *Roll back* to the deployment before (auto-sync off, as
  Argo CD requires; confirmed, write mode applies). *In cluster*: the revision of each source
  deployed with its commit's message, author and time (a chart's version has none). *Problems*:
  from the resource tree, one row per workload whose pods are in trouble (`CrashLoopBackOff`,
  `Pending`, `ContainerCreating`, *Not ready*...), with how many of its pods, since when and *why*
  (Argo CD's health message), then other objects Degraded (an autoscaler with no metric), then,
  with auto-sync on, what should be live and isn't (*Missing*, from the Application's status:
  the tree holds only live objects); the pod count and *all pods* (the destination namespace on
  its context, else the tree's pods). *Nothing wrong.* is said only from the tree or of a Healthy
  Application; from a status without health per resource (Argo CD 3) it says *No detail* and why.
  *Last sync*: phase, message, duration, who, when and its steps (each resource's result and a
  hook's phase), progress live while it runs. *Sync diff*: how many resources a sync would change
  (`~ changed, + added, – pruned`, with names) and *show* for Argo CD's diff, or *Nothing to sync*.
  *History*: the last deployments as dots, a sync that failed after them red, counts and the
  list. Only the tab shown is read, by Argo CD's API (the Application, `resource-tree`,
  `managed-resources` and `revisions/<rev>/metadata`, a commit kept once read). It is read again
  when its row changes (the stream; a change during a read is read once that read ends), 3 s after a read that found a sync running (when
  the page is hidden, once it shows again), and on *Refresh*. Leaving the tab or the page stops
  the read running; the page shown again reads the tab anew. The cards are worked out off the UI
  thread.
- **What changed between two syncs**: in the History card's list, *diff* on a deployment compares
  it with the one before, and *pick* on two deployments compares them; the comparison opens under
  the cards. *Sources*: what each deployment asked for, from Argo CD's history (the revision
  deployed, Helm values — `valuesObject` wins over the values text, as in Argo CD —, parameters
  and value files, kustomize images and patches, a plugin's env), shown like Helm's values diff
  (only changed lines, folded context, `-`/`+`) under a summary. Nothing there is masked: Helm
  values, parameters and a plugin's env show as the Application holds them. *Rendered manifests*:
  what Argo CD renders at each deployment's revision (`GET
  /api/v1/applications/<name>/manifests?revision=`, or `sourcePositions`/`revisions` pairs for
  several sources), from the Application's sources as they are now (a change of its own
  parameters shows under *Sources*). A deployment made from another shape of sources (one source
  then several, another count) or an Application with a source hydrator can't be rendered as it
  was: the comparison says so rather than compare; a source with no revision recorded is said
  (rendered at its target revision now). Both are read by a smart diff:
  a summary first (`image web 1.15.0 → 1.16.0`, `replicas 3 → 5`, `env FOO added`, `config key
  app.yaml changed`, `2 resources added`), each change once with the resources it is in (one
  image bump in 12 Deployments is one line; the first 50 changes and 200 resources listed); then
  noise folded away and counted (`12 noise changes hidden (checksum annotation 6, chart label
  6)`, *show* to see it): `checksum/*` annotations, `helm.sh/chart`, `chart` and
  `app.kubernetes.io/version` labels, `pod-template-hash` and `controller-revision-hash`, an
  image's digest when its tag changed too (a digest alone is said: *digest changed*), timestamps
  in the object's own metadata, the status, the field managers, the last-applied annotation and
  what the API server sets. A tag is never noise, however much it looks like a hash or a date, nor
  is a timestamp in a pod template (`restartedAt`), env or data. A ConfigMap or Secret kustomize
  regenerated under a new name (its 10-character content hash) is the same one (*renamed a → b*),
  not one pruned and one added, and references to it that changed only by that rename are noise.
  A pod template where only noise changed says *pod template changed (pods restart)* with the
  keys (over Argo CD's API a Secret's values are hidden, so a `checksum/secret` annotation may be
  all that tells it changed). Then a row per resource that unfolds its diff. A Secret's values
  are never shown: only *Secret key X changed*, added or removed (also in a `List`; Argo CD hides
  them in what it renders with runs of `+`, one length per value, so a value is said changed
  only when its run differs). Only Secrets are masked: a
  value that looks like a password in another kind is shown. Read on demand, when a comparison
  opens, off the UI thread (Argo CD may take up to 2 minutes to render); each revision's manifests
  are kept once read, under the Application's sources as they are now (the last 16, 64 MB at
  most), and a newer comparison or another tab drops the read running.
- **Argo CD grouped diff**: on the Argo CD page, pick Applications with the checkbox of their
  row, a group at a time (the checkbox of a service's heading; *Pick the group* in an app of
  apps' menu, for it and those under it), or *Pick all filtered*; picks are kept by Argo CD,
  namespace and name across reads of the list, and *Clear* drops them. An action on the picks
  works on those the filter keeps: the others are counted (*12 picked (3 hidden by the
  filter)*), never acted on. *Diff selected* reads what a sync of each would change now: Argo
  CD's own diff (`GET /api/v1/applications/{name}/managed-resources`: the live state against the
  predicted one, else the target state; hooks left out, as Argo CD usually does already), read
  with the smart diff above (noise folded, Secrets masked). A resource Argo CD says differs where
  the smart diff finds no change is said, never taken as in sync. An Application Argo CD couldn't
  compare with git (a `ComparisonError`, `InvalidSpecError`... condition, or a sync status
  Unknown) is not read: it is listed with why. Then the changes are grouped across Applications:
  an image, digest, replicas, env var or pod template change is one line wherever it is (`image
  web 1.15 → 1.16 (12)`); a change of a resource's own (a ConfigMap's key, a field, a resource
  added or pruned) is one line for the resources of the same group, kind and name with the same
  values (`ConfigMap app-config key LOG_LEVEL changed (12)`; to two values, a line each, *one of 2
  different values*; a core ConfigMap kustomize regenerated by its name without the hash; a kind
  of another group with a core kind's name says its group, `Service.serving.knative.dev`). A
  summary on top (`3 changes in 12 environments: …`); each line says the Applications it is in
  (*in all 12*, or *in 2 of 12:* and which), images first, the change in the most Applications
  first; a click unfolds each Application's resources and their diff (worked out when read, off
  the UI thread). *N noise changes hidden* (*show* adds the noise by rule, and in the lines). Then
  the Applications with nothing to sync, with only noise, those Argo CD says differ with no change
  shown, those it couldn't compare and those that could not be read, the same error once with its
  Applications (one failing never hides the others). Reads run 6 at a time, off the UI thread,
  and the lines are regrouped as reads end, at most every 150 ms (*Read 5 of 12…*); at most 300
  Applications are read at once (the others are counted), 100 changes, 20 Applications per
  change and 10 resources per Application are listed. Each Application's diff is kept a minute
  by its resourceVersion (64 MB at most), so diffing the same picks again reads only those that
  changed; *Read again* reads them all anew; *Applications* (back) drops the reads running.
  Read-only.
- **Argo CD grouped sync**: *Sync selected…* (next to *Diff selected*, or *Sync these…* on the
  grouped diff) opens the sync of the Applications picked that the filter keeps (those it hides
  are counted, never synced; 300 at most). Each is read first by Argo CD's API, 4 at a time: one
  already syncing, being deleted, or whose sync windows deny a manual sync now (*can sync in 2h*)
  is left out, saying why; auto-sync on is said, not a reason. When its sync windows aren't known
  (its project can't be read), argocd-server refuses a sync a window denies and says so. Windows are worked out again as of
  the moment *Sync…* is clicked. Options: *Full, with hooks* (Argo CD's hook strategy, its own
  Sync: the whole Application, PreSync, Sync and PostSync hooks run, listed under each as its
  last sync ran them) or *Out of sync only, without hooks* (the apply strategy on each
  Application's resources out of sync, named in the sync: what is in sync is not applied
  again, one with nothing out of sync is left as it is, and the hooks skipped, a secret refresh
  or a migration, are listed under each and in the confirmation; for a simple change);
  *Prune*, off by default, lists under each Application every resource it deletes (its managed
  resources that require pruning), and a sync is refused if Argo CD's last comparison then
  shows more to prune; *Dry run* applies nothing. With auto-sync off and one source, the sync is
  pinned to the revision Argo CD compared (`status.sync.revision`, shown on the row), so git
  moving on can't bring other changes; otherwise it goes to the target revision. As argocd-server
  does, the Application's sync options and retry policy go with the operation, and prune and dry
  run are always said (false too, so Argo CD's manual sync defaults don't fill them). *Read
  what a sync of each would change* shows the grouped diff, whose summary then shows above.
  With Applications of production environments among them (the types `prod`, `preprod`, `acc
  on a tag`, `platform`, or one named with `prod`), they are named above the list and *Sync…*
  asks a second click (*Confirm: 3 production Applications…*) before the confirmation.
  The page opens in read-only mode too, saying so: everything above shows, and *Read what a sync
  of each would change* reads the diff, without turning writes on. Only *Sync…* (or *Terminate*)
  asks to, with a prompt of its own: what turning writes on allows here (*sync 17 Applications
  on argocd.example.com*, through Argo CD's API with your Argo CD login, not your kubeconfig) and
  that write mode is the whole app's. A single Application's Sync, Refresh, Terminate or Roll
  back by Argo CD's API asks the same way.
  *Sync…* asks for the count of Applications typed (or the wait set in Settings → General),
  names them, and lists everything prune deletes; the syncs are then sent 4 at a time, write mode
  and the server's protection checked as each is sent. Each Application is read every 2 s (every 10 s while the page is
  hidden or more than 50 run) until its operation ends, at most 30 min: its phase (*Waiting*
  until Argo CD starts the operation after the one read right before the write, so an auto-sync
  in between is not taken for it), message and resources done (`3 of 5 done`, a bar),
  *Terminate* while it waits or runs (confirmed like a protected context), then *Done: 10
  succeeded, 2 failed*, each failure with Argo CD's message. Leaving the page while it runs only hides it:
  the selection bar shows *Sync of 12 Applications: 3 running* to show it again. Agents sync
  several Applications by calling `gitops` `sync` once for each (each decided by their rules and
  approved like a single sync; `hooks: false` syncs without hooks).
- **Argo CD sync journal**: every operation the app sees (in an Argo CD API server's stream, a
  dashboard, a grouped sync) is kept on this machine: the Application,
  its Argo CD, revision(s), phase, message, who started it, when it started and ended, dry run
  and strategy, and each resource's result (kind, namespace, name, status, message, hook phase
  and type, sync phase). These are stored as Argo CD says them, for 30 days: operation and step messages
  (which may quote what a resource or hook printed) and the usernames of who started each.
  One JSON line per state of an operation, a state seen again not written again (a cheap
  check of its phase, end, message and each step's status comes first), at most 256 KB (past
  that, step messages are left out, then the last steps), in `argocd-sync-journal.jsonl` in
  the data directory (`~/.local/share/recouvrance`, mode 0600); past 25 MB it moves to `.1`, so
  50 MB at most, and what is older than 30 days is left out. Each resource's result is asked
  of the stream only, never of the list of every Application.
  *Sync journal* (the selection bar, the sync page, *journal* on a dashboard's Last sync and
  History cards for that Application) lists them newest first, filtered by Application, phase,
  who and since when; a click shows an operation's message and steps, even after Argo CD's
  history dropped it. The cause comes first: the steps that didn't succeed (failed, running,
  waiting, skipped), by phase, each with its message and how long it ran as the app saw it,
  and for a sync running or one that timed out the phase it was in (Argo CD's result doesn't
  say the wave). The succeeded steps fold into one line (*438 succeeded: 120 ConfigMaps, 60
  Secrets…*), unfolding by kind, one line per step; a message that only says the step was
  applied (`configmap/web serverside-applied`) is left out. *Steps: status, kind, name…*
  filters them. The dashboard's Last sync → steps shows the same. A line that doesn't read is skipped and counted (*2 lines of the journal
  could not be read and were skipped*), never breaking the rest.
- **Argo CD sync, terminate and roll back**: *Sync* (the Application's page, its row menu, the
  Argo CD panel) reads the Application first and lists its resources out of sync, all ticked:
  untick some to sync only the others (hooks run only when all are synced; past 200 the whole
  Application syncs). *Prune*, off by default, names exactly what it deletes (out of sync and no
  longer in git, of the resources ticked), and a sync that would now delete more is refused;
  *Dry run* applies nothing, and what Argo CD would do is shown when it ends (waited for up to
  30 s). *Terminate* stops the operation running, as `argocd app terminate-op` does, also when it
  holds the Application's deletion. *Roll back…* on a past deployment (the Application's page, the
  Argo CD panel, the row menu for the previous one) applies that deployment's revision and
  sources again, without hooks or prune, as `argocd app rollback` does: the deployment the dialog
  names is the one sent, even if another lands meanwhile. With auto-sync on it is disabled, since
  Argo CD refuses it (revert in git instead). Like argocd-server, nothing starts while another
  operation runs or the Application is being deleted, and the write carries the version of the
  Application just read, so a change meanwhile refuses it. Each dialog shows the `kubectl patch` it sends; write mode,
  the hub's protection and *Run as* apply.
- **Argo CD resources**: an Application's page lists what Argo CD tracks for it
  (`status.resources`) as a table: kind, namespace/name, sync, health, whether it is a hook, and
  a message (its health message, and *no longer in git* for one out of sync that a sync with
  prune deletes; not a hook left from the last sync, nor a resource Argo CD couldn't compare).
  *Out of sync or unhealthy* shows those alone. A row opens the object on the kubeconfig context
  that reaches the destination, with the kind and version that cluster serves, connecting to
  it when it isn't connected; when no context reaches it, the rows don't open and the page says
  so. The rows come from the Application itself, with no request per row, and only those on
  screen are drawn: an Application with thousands of resources stays fast. When Argo CD keeps
  no health per resource in the Application (`status.resourceHealthSource: appTree`, the
  default from Argo CD 3.0 unless `controller.resource.health.persist` is on), the page says so.
- **Argo CD diagnosis**: an Application's page says what is wrong with it, worst first, each
  with its objects (opening on the destination's context, connecting to it when it isn't) and
  what to do:
  - *Being deleted*: the finalizers holding it (what each one waits for) and the resources not
    deleted yet, read live on the destination when it is connected; red after 5 minutes.
  - *Sync running* for more than 10 minutes: the hook or resource it waits for (*Terminate* stops it).
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
- **Argo CD's own diff** (off until set up): with the hub context's Argo CD server set in
  Settings → Per cluster, an Application's page offers *Show Argo CD's diff*: what a sync would
  change, as Argo CD renders it from git (Helm and Kustomize applied), against the live objects.
  Each resource that differs (changed, created by the sync, or no longer in git) is shown as a
  diff from live to git, the first ones open and the others a click away; hooks are left out and
  Secret values are hidden. It is one read-only request to that server, sent on the click, with
  the argocd CLI's login to it or a read-only token set in Settings (see
  [configuration](configuration.md#argo-cds-diff-argocd_servers)).
- **Assistant beside any page** (Ctrl+J, ⌘J on macOS, or the sparkles button in the top bar): the
  window's assistant chat opens on the right of the page shown, the same chat as on its own page (the
  expand button opens it there). It is told what is on screen, as its header shows: the
  clusters and namespaces picked, and the object whose page is open (with its context), or the
  list with its filter and the row selected, or the page (Problems, Timeline, overviews, Helm,
  Argo CD). A page's *Investigate* asks it beside the page when it is open there.
- **Overviews**: cluster capacity and health (requested is what the pods bound to a node hold there; pending pods no node took are said apart, with what they ask for), workloads status, Helm releases (values masked; install from your repos, roll back and uninstall through your `helm` CLI).
- **API requests** (Settings → API requests): per cluster, how many requests of each kind
  (discovery, lists, watches, metrics, logs, shells, writes) were made and how many failed; how
  long the API server took to start answering (p50, p95, slowest) and, for answers that end,
  until their last byte was read (p50, p95); and how much each kind received per minute over
  the last hour, counted decompressed as the bodies arrive. The time to the last byte includes
  Recouvrance's own parsing when that sets the pace. Watches, logs, shells, attach and
  port-forwards are streams that run until closed: bytes only. A shell's, an attach's or a
  port-forward's data goes over the upgraded connection, and counts as it is read: what came
  back through it, not what was sent. Below, the 10 heaviest requests, one per kind (cluster,
  method and path, its names replaced by `{namespace}` and `{name}`), with their bytes and both
  times. A stream is listed once it ends: a port-forward, each connection on its own. Nothing
  leaves the machine.
- **What's new and updates**: the first start after an update shows the release notes of every
  version since the one run before (the version's own notes when that one is unknown), once.
  They are the changelog built into the app: nothing is fetched. Settings → About shows them again,
  says how the copy you run was installed (the install script, the `.deb`, the AppImage, or a
  tarball or source build, with its path) and how to update it, with the command to copy.
  *Check for updates* there asks GitHub for the latest release, only when clicked, and says
  whether it is newer.
- **Favorites**: star contexts (in the context picker), namespaces (in the namespace picker),
  Argo CD Applications (on each row of the Argo CD page and on an Application's page), objects
  (on a detail page, or *Add to favorites* in a row's menu) and resource types (in the sidebar).
  `*` (outside text fields), or *Add to or remove from favorites* in the palette, stars or unstars
  what is shown: the object or Application of a detail page; on a list, its selected row, else
  the one namespace picked. Other pages have nothing to star this way (the Argo CD page has a
  star on each row). Favorites come first where they are picked: the context picker lists
  favorite contexts first; the namespace picker lists first the favorite namespaces of the
  contexts connected; the Argo CD page sorts the trees holding a favorite first and its
  *Favorites* toggle keeps only them. With several contexts combined, the namespace picker shows
  one row a namespace: its star is lit when the namespace is a favorite on any of them, starring
  marks it on each, and unstarring removes it from each (favorites of contexts not connected are
  left as they are). The sidebar's *Favorites* section lists them all, on any context, and so
  does the palette (type `favorite` to list them). A click never replaces the contexts
  connected, unless it is a context's: a context connects alone, as picking it does; a namespace
  is picked, its context added to those connected first when it isn't one of them; an object or
  Application opens its page, on a context that isn't connected with a client of its own, as the
  Argo CD page opens Applications (a page the user went to while it connects stays on screen); an
  Application read only through Argo CD's API opens the Argo CD page. One whose target is gone
  says so: a context no longer in the kubeconfig, a type the cluster doesn't serve, an object
  deleted since (its page's *Deleted from the cluster*); a namespace its contexts don't list is
  picked all the same, with a warning, as a user who may not list namespaces can still use one.
  An Application is the same favorite whichever way it is read (see
  [configuration](configuration.md#favoritesjson)). Every window shows and changes the same
  favorites, kept in `favorites.json`.
- **Saved views** (type + namespaces + filter, bookmark button), back/forward history, light and dark themes
  whose status colors, badges and status buttons read at WCAG AA contrast (4.5:1) on what they are drawn on.
- **Instant back**: the last 4 lists left in a window (up to 60,000 rows in all, for 5 minutes) keep their
  watches, paused like a hidden page's, so going back to one (by kind, tab or namespace) shows its
  rows at once, current within half a second, without listing it again. *Refresh* lists it again.
  Other lists seen recently (the last 8, up to 10,000 rows in all) show at once, marked
  "Updating…", while a fresh list loads and replaces it in one go.
- **Bulk actions**: check rows, then read their logs together, restart, scale, delete or copy names. Scaling needs the number
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
  debug pods with an ephemeral container (falls back to a Pod Security "restricted" profile), *Evict* a
  pod (through the Eviction API, like a drain: refused when a PodDisruptionBudget would be broken,
  and the refusal names the budget), run a CronJob now, suspend/resume (CronJobs, Flux, Argo CronWorkflows), drain nodes, force delete
  or remove finalizers (only once the delete leaves it stuck terminating: it already was, or still is
  after 10 seconds), delete a Deployment, StatefulSet, DaemonSet, ReplicaSet or CronJob and keep
  what it owns (a *Keep ... (orphan them)* box in its Delete dialog, such as *Keep its ReplicaSets and pods* for a
  Deployment; `kubectl delete --cascade=orphan`: how a StatefulSet's volume claim templates or a selector are changed
  without stopping its pods; not together with removing finalizers, which would delete what it keeps), an ExternalSecret's *Force
  refresh* (its `force-sync` annotation: the operator fetches it from its store now), Flux *Reconcile*, Argo CD
  *Refresh*/*Hard refresh*/*Sync*/*Terminate*/*Roll back*, create/apply YAML (a server dry run first, then a confirmation
  showing what each object changes against the cluster: created, changed with its diff, or no change). YAML edits on a detail
  page save only that object: a changed kind, API group, namespace or name is refused, and so are masked Secret values. The
  confirmation shows the diff of the dry run, and the save is sent as of the version the edit started from: when the object
  changes on the cluster meanwhile (an HPA, a controller, Argo CD), a warning over the editor says who changed it, and
  offers *Compare*, *Keep my edits on the latest* (your changes made on the object as it is now) and *Reload*, instead of
  undoing that change. Secret values never show in these diffs. Every
  confirmation shows the equivalent `kubectl` command. A page whose object is deleted says so, and
  shows the one created again under its name, if one is.
- **Run as** (Settings → Contexts → *Run as*): set a user (and groups) a
  context's writes may run as, like `kubectl --as`. Write confirmations then show a *Run as
  `<identity>`* checkbox, off each time; checked, that one action runs as the identity and the
  `kubectl` line shows `--as`. A protected context still asks for its confirmation (a wait or its name). Reads go as you; write mode and
  the identity are checked again when each request is sent (an identity changed meanwhile sends
  nothing). Actions disabled because your RBAC forbids them stay enabled when the context has an
  identity. Covered: scale, restart, rollback, delete (force, finalizers, orphan), YAML save, the values
  editor, Apply, cordon/uncordon, drain, evict, ExternalSecret refresh, CronJob run, suspend/resume, Flux reconcile, Argo CD
  refresh/sync/terminate/rollback, role bindings. Not covered: Helm, bulk delete from tables, plugins, shells, debug,
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
- **Read-only contexts** (Settings → Contexts → *Read-only*): the window never writes to them, even
  with write mode on. Their writes, shells, attach, Helm changes, Argo CD actions and writing
  plugins are refused when sent ("prod-eu is read-only"), and a bulk action on checked rows that
  include one does nothing and says so. The local kubectl terminal doesn't open on them (its
  kubectl could write). Reads, logs, port-forwards and dry runs still work. For Argo CD, the
  hub's setting decides (or the API server's name, when no hub is tied to it), not that of the
  cluster an Application deploys to: a sync from a writable hub still deploys there. Agents
  can't write to them either, whatever their rules say: the refusal is final, and `check_access`
  and the Agents page's test say so. The context picker marks them `read-only`.
- **Diagnosis** on pods and workloads: why it is broken (crash output and exit code, scheduling,
  image pulls, probes, volumes) and what to try. On a workload's page it also says where the
  failure comes from, from its pods' revisions and nodes: the new revision (only its pods fail,
  the previous one's are ready), one node (only pods there fail, the same revision is ready
  elsewhere) or the app (old and new revisions fail, or every pod fails on several nodes). A pod
  fails when it crashes, exits with an error, cannot pull or create a container, or is evicted; a
  pod still starting, warming up behind its readiness probe or waiting for room on a node does not
  count, so a rollout or scale-up in progress names no cause. It also shows the worst pod's crash
  output (one log read, only when a container of that pod crashed).
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
  filesystems, local storage left in TopoLVM volume groups (used of total per device class), DRA
  devices; ephemeral storage per pod.
- **YAML**: templates (plus your own in `~/.config/recouvrance/templates/`), open/drop files, load an
  https URL, edit in `$EDITOR`, errors located on their line, field docs (`kubectl explain`) and forms
  generated from the cluster's schema, managedFields toggle, download as YAML.
- **Logs**: find (regex, match case, next/previous) on top of the filter (`!word` excludes).
- **Tables**: column expressions in the filter (`status:Running restarts>3 cpu>200m age<1h`), hidden
  columns included (`ip:10.4.2.17`), filter history and saved searches, links from cells to the
  objects they name, click a namespace to filter, CSV export, tabs. A big list in every namespace
  is read without its objects' labels; a filter on labels (`app=web`) loads them once, and the bar
  says "Loading labels…" meanwhile.
- **Port-forwards** with options: local port, listen address, https, open in the browser; they follow
  replacement pods.
- **Per-cluster settings**: Prometheus or VictoriaMetrics endpoint, node shell image, accessible
  namespaces (when you may not list them), metrics off, an identity to run as; namespaces can also be added by hand.
- **Kubeconfig**: extra files and watched folders, reloaded on change; contexts with the same name in
  two files are kept apart as `name@file`. A file that fails to parse is reported by its path, line
  and column, never by what it holds there.

The **command palette** (Ctrl+K, ⌘K on macOS, or the search box of the top bar) finds resource
types, pages, contexts and objects across the connected clusters, and runs every command, with a
key or without: Upgrade check, API requests, Agent activity, My access, write mode, the theme,
each Settings section and each saved view. It knows other words for them too (`rbac` finds My
access, `deprecated` the Upgrade check, `audit` Agent activity) and shows each command's key. A
word of the detail line finds an object, and words combine: `web prod` finds `web` in the prod
namespace, and a name with a cluster's name finds it on that cluster. Letters spread out only
match its name.

The keyboard shortcuts, as `?` (or the keyboard button of the top bar) and Settings → Shortcuts
list them; the palette, that list and this table are all made from one list of commands:

| Linux | macOS | Command | Where |
|---|---|---|---|
| Ctrl+K | ⌘K | Command palette: resources, pages, contexts, objects and every command | Anywhere |
| ? | ? | Keyboard shortcuts: this list | Outside text fields |
| Ctrl+Shift+N | ⇧⌘N | New window | Anywhere |
| Ctrl+T | ⌘T | New tab: right-click a row to open it in one | Anywhere |
| Ctrl+W | ⌘W | Close tab | Anywhere |
| Ctrl+Tab or Ctrl+PageDown | ⌃Tab or ⌃PageDown | Next tab | Anywhere |
| Ctrl+Shift+Tab or Ctrl+PageUp | ⌃⇧Tab or ⌃PageUp | Previous tab | Anywhere |
| Ctrl+B | ⌘B | Hide or show the sidebar | Anywhere |
| F5 or Ctrl+R | F5 or ⌘R | Refresh: in a terminal, F5 goes to the program | Anywhere |
| Alt+← | ⌘[ | Back | Anywhere |
| Alt+→ | ⌘] | Forward | Anywhere |
| Esc | Esc | Back to the list | On a detail page |
| Ctrl+= | ⌘= | Zoom in | Anywhere |
| Ctrl+- | ⌘- | Zoom out | Anywhere |
| Ctrl+0 | ⌘0 | Reset the zoom | Anywhere |
| Ctrl+F or / | ⌘F or / | Filter the table: `web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · `/^api-\d+/` · `status:Running` · `restarts>3` · `cpu>200m` (all terms must match) | In a table |
| j / k / Enter | j / k / Return | Move the selection, open the selected row | In a table |
| Right-click a row | Right-click a row | Row menu: open, logs, shell, YAML, events, copy name, favorite | In a table |
| g then p | g then p | Problems: what is broken, across the clusters | Outside text fields |
| g then o | g then o | Cluster overview: nodes, capacity and versions | Outside text fields |
| g then w | g then w | Workloads overview | Outside text fields |
| g then h | g then h | Helm releases | Outside text fields |
| g then i | g then i | Assistant: ask your clusters | Outside text fields |
| g then s | g then s | Settings | Outside text fields |
| * | * | Add to or remove from favorites: the object or Application shown; on a list, the selected row, else the one namespace picked | Outside text fields |
| Ctrl+J | ⌘J | Assistant beside the page: it is told what is on screen | Anywhere |
| Ctrl+Shift+V | ⌘V | Paste in a shell | In a shell |
| Double-click a context | Double-click a context | Connect to just that context | On the Contexts page |

Single keys (?, *, g, j, k) work when no text field or terminal has focus.

Tab and Shift+Tab move the focus through the top bar, the sidebar (pages, sections, resource
types, saved views and favorites), the cards of the Contexts page and the sections of Settings;
Enter or Space opens what has focus, and a tint shows where it is. Screen readers (Orca,
VoiceOver) read each of them by name, the buttons that show only an icon included, along with
the page or section picked, the sections folded and the contexts ticked. The palette's results
are a list, with the highlighted one marked as selected.

On macOS, ⌘ takes Ctrl's place, so Ctrl keys reach the shell in a terminal (Ctrl+Tab still
switches tabs), and the menu bar has the keys every Mac app has: ⌘, opens Settings, ⌘Q quits, ⌘W
closes the tab or, at the last tab, the window, ⇧⌘W closes the window, ⌘M minimizes, ⌘H hides,
⌘R refreshes and ⌘/ lists the shortcuts. Its Edit menu undoes, cuts, copies, pastes and selects
all in a text field, and copies the text selected on a page; ⌘V pastes in a shell. The app keeps
running once its last window is closed, as Mac apps do: its Dock icon or File → New Window opens
one.
