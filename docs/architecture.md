# Architecture

Recouvrance is one Rust binary: a [gpui](https://github.com/zed-industries/zed) window (through
`gpui-kit`) on top of [kube-rs](https://kube.rs), plus a local MCP server ([rmcp](https://github.com/modelcontextprotocol/rust-sdk)).

## Threads

- The **UI thread** runs gpui: views are `Entity`s, rendered every frame they change.
- Kubernetes calls run on a **tokio runtime** (`k8s::run`, `k8s::spawn_guarded`). Long-lived work
  (watches, log streams, port-forwards) returns a `TaskGuard`: dropping it cancels the work, so a
  page that goes away stops its streams.
- Pages hidden in the back/forward history pause their polling (`app::wait_visible`), and so does
  every page of a window hidden as the desktop reports it (gpui's window visibility): minimized or
  on another workspace, and on macOS also fully covered by other windows or on a display that
  sleeps. X11 sees a covered window only without a compositor, and Wayland compositors without the
  xdg-shell `suspended` state never report a window hidden. The list then holds its rows back
  (a first list that completes meanwhile says so only with them) and stops its metrics, and the
  sidebar's problem count starts no scan (an agent's `find_problems` still does when its
  findings are too old). Watches, log streams and shells keep going, so a window shown again is
  current at once. A window shown on another monitor, not focused, keeps updating.
- A list left for another kind, tab or namespace keeps its watches the same way (`app::kept_lists`:
  the last 4 of each window, up to 60,000 rows in all as counted when each was left, for 5 minutes,
  as long as the pods' requests watch lingers off screen; the least recently left dropped first), so
  going back to it shows current rows without a list request. Its tasks carry the id of their list
  and touch the table only while it is the one on screen.
- The Timeline's events are listed once, then watched (`k8s::watch_events`); only the newest
  20,000 of a cluster are held. Their watch is the exception to the rule above: it closes while
  the page is off screen, and once the page is shown again it goes on from where it was, bringing
  what changed meanwhile, or lists again when etcd no longer has that far back. Where the user
  may list events but not watch them, they are listed again at each overview refresh.
- The page on screen gets the API servers first. Background work (the problem count, a hidden
  list) takes at most 8 of the 32 slots that requests by namespace share (`k8s::lane`). Each
  request takes its slot in the lane of that moment, so a list that comes on screen while it loads
  gets the page's slots for the rest. The first problem count waits for the list on screen to show
  its rows (15 s at most), and the pods' requests and limits for the usage bars are read after the
  list's first rows.

## Source map

| Path | What |
|---|---|
| `src/main.rs` | Entry point: the window, `serve` (headless MCP), `agent-create` |
| `src/app/` | The main window: contexts, sidebar, resource list page, tabs and history, theme, key bindings and the macOS menu bar |
| `src/table/` | The resource table (server-side columns, usage bars, links) and its filter language |
| `src/details/` | A detail page: tabs, actions and their confirmations, logs, compare, related data |
| `src/overview/` | The Overview tab, per kind: pods, workloads, nodes, networking, configuration, RBAC |
| `src/k8s/` | Everything that talks to the API server: kubeconfig, discovery, tables, exec, port-forwards, metrics, writes |
| `src/mcp/` | The agent hub (agents, rules, approvals, audit) and the MCP server with its tools and prompts |
| `src/agents/` | The Agents page |
| `src/ui/` | Other pages and widgets: settings, Helm, apply, cluster and workloads overviews, terminal, pickers |
| `src/diagnose.rs`, `rbac.rs`, `schema.rs`, `prom.rs`, `problems.rs`, `certs.rs`, `updates.rs` | Logic without UI, unit-tested |
| `src/dev/` | Developer tools (feature `dev`): the off-screen snapshot harness and live smoke checks |

## How lists stay fast

Lists use the API server's **Table** format (`kubectl get`'s columns, computed server-side), then
are **watched**. A list of a namespaced kind in every namespace comes in two steps (`list_fast` in
`src/k8s/table.rs`):

1. Its first page (2,000 rows), shown at once.
2. When more follow, every namespace's rows without their objects, read from the API server's cache
   at least as new as the first page, 32 namespaces at a time (`cells_by_namespace`): about 6 MB
   instead of 160 MB for 20,000 pods. These rows have no uid or labels, and a creation time read
   from their Age. The list is complete there: its watch starts from the first page's version, so
   it replays every change since, and each event gives its row its uid and labels.

The metadata of every row is downloaded only when something needs it: a filter on labels makes the
watch list again with it, in one response from the cache parsed as it streams in, with
managedFields and the rest skipped (`list_whole`), and so does a list the Timeline records. A delete
from such a row reads the object's uid first, and leaves alone an object newer than the row (one
re-created under the same name since the list).

Rows keep only the metadata they need; full objects are fetched only when a custom column needs
them. Responses are gzip-compressed. The usage bars and the cluster overview take the pods'
requests and limits from one list of the pods, then follow them with a watch
(`src/k8s/pod_requests.rs`). The API server can't send less than whole pods (about 300 MB of JSON
for 20,800 pods), so each pod is kept only as the sums `podspec` makes of it, and pods that come,
go or are resized in place change them without another list. A page off screen for 5 minutes
stops its watch and lists again once back. On a cluster with about 20,800 pods, the first rows
show after 0.7 s and all of them after 1.4 s ([measured](COMPARISON.md#measured)).

The Problems scan reads the big kinds from their cells too (`list_light`). What its checks need
of whole objects (Services, Ingresses and HTTP routes, the pods a Service selects, the workloads of
a namespace with an idle Service, external-secrets.io's objects) is parsed as each list streams in,
keeping only the fields the checks use; of the workloads, only their pod templates' labels are kept. A
Service's verdict is kept by its uid and resourceVersion for 10 minutes, so the same idle Services
don't cost a pod list each on every scan. A scan asked for findings from a given time on (Refresh,
an agent's `max_age_seconds`) judges again the Services judged before it.

One scan runs per cluster and scope (`problems::shared`), whoever asks: the sidebar badge of each
window, the Problems page and the agents' `find_problems` read its latest findings, kept check by
check as each finishes, and wait for the scan running rather than start another. A scan is due
eight times as long as the last one took after it ended (2 minutes at least); a page scoped to some
namespaces reads them from the cluster's scan when there is one that covers them and could read
everything, but scans only those namespaces for a Refresh or before the cluster's first scan; a
user who may not list cluster-wide gets those namespaces scanned on their own. The sidebar's scans
run in the background lane; one the page, Refresh or an agent waits for is raised to the
foreground (`lane::Raisable`), its requests waiting for a background slot included. A scan
stopped half way says so, and is never taken for an all-clear.

The Workloads overview counts each kind from kubectl's cells alone, with no object metadata at all
(`list_cells`), in one request from the cache. The pods card works differently, as it needs each
pod's namespace past 4,000 pods: it reads a first page of at most 1,000 rows (paged, so etcd serves
it), and past that one namespace at a time from the cache. It reads pods whole only up to 4,000 of
them, for their restarts and limits, parsed lean; HPAs, Disruption Budgets and Helm's current
revisions come lean from the cache too. kube-apiserver keeps no cache of events by default, so the
Warning events are read 5,000 at a time and added up by reason as each page streams in
(`warning_reasons`): only the reasons are held, however many events the cluster keeps.

A detail page follows its object the same way (`k8s::follow`): a list, then a watch, with a field
selector on its name, which the API server serves from its watch cache. The pods it lists are
followed by their label or field selector (a node's by `spec.nodeName`), whole but without their
managedFields. A change shows about a second after it happens, and an idle page asks nothing for
them but a watch opened again every few minutes. Its watches stop while the page is in the
back/forward history and hold back what they send while its window is hidden. Usage
(metrics-server, the kubelet) and the charts and alerts (Prometheus) are read every
`metrics_refresh`; what the page links to (Services, revisions, events…) again when the object or
its pods change, and at least every four turns. An object that can't be watched but can be
listed is listed every `detail_refresh`; one that can only be read (a role that only grants `get`)
is read every `detail_refresh`.

## Safety model

- **Read-only by default.** Write actions ask to enable write mode first.
- Every write is **confirmed**, shows the equivalent `kubectl` command, and most are validated with
  a **server dry run** first.
- **Protected contexts** require typing the context name.
- Actions your **RBAC** forbids are disabled (`SelfSubjectAccessReview`).
- **Agents** go through the hub: rules (deny, ask, allow), approvals (which show what a write will
  change, worked out with a server dry run), a rate limit, an audit log and a kill switch. Write mode
  and protected contexts don't apply to them: only their rules do (an Ask or Deny rule can name
  `@protected`).
- **Login commands keep their secrets.** kube's error for a failed exec plugin holds its whole
  command line and its output; each client replaces it, when it is built and on every request
  (a token refresh runs the plugin again), by the program's file name, taken from the kubeconfig,
  and its exit code (`k8s::login`). A plugin that cannot start is reported with its name too. The
  last line the plugin wrote to stderr is added when the kubeconfig sets `interactiveMode: Never`:
  kube keeps stderr only then, and otherwise the plugin writes to the terminal Recouvrance was
  started from. Pages, agents and the audit log only ever get that.
- **Kubeconfig errors keep their secrets too.** The YAML parser's error for a kubeconfig shows
  the lines around where it failed and can quote a value: a token, a client key, a login
  command's arguments. Kubeconfig files are read through `k8s::read_kubeconfig_file`, pasted
  text through `parse_kubeconfig`: their error says the file, line and column, and why in words
  of their own. A file that starts with a UTF-16 byte order mark and has an odd number of bytes,
  which kube's reader panics on, is an error of that file too, not a crash.
- **Proxy passwords stay out of errors.** kube's error for a proxy it can't use prints the proxy's
  URL whole, user and password included. The client's build error puts that URL back as its
  scheme, host and port alone, scheme or not (`user:password@host:1080` from `HTTPS_PROXY` reads
  as a URL to kube, not to a text), whatever the password holds: a proxy's URL has no path or
  query, so everything up to the last `@` goes, and everything after the host. kube keeps the
  URL as an `http::Uri`, which drops what follows a `#`: the password left before it can read
  as a host and port. So when the proxy's text, as kube reads it (the cluster's `proxy-url`,
  else `HTTPS_PROXY`, else `https_proxy`), holds a `#`, only the scheme is shown. From the
  `Uri` alone, a URL with a scheme is shown by its scheme too when its port is no number, when
  it has a user and no port, or a path or query and no user. The build error also leaves out
  the user and password of any other URL in its text, as of one in the login plugin's stderr
  line (`k8s::login`), but there a password with an unescaped `/`, `?` or `#` still shows
  (#1000).
