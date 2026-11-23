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
  sidebar's problem count waits. Watches, log streams and shells keep going, so a window shown
  again is current at once. A window shown on another monitor, not focused, keeps updating.
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
| `src/diagnose.rs`, `rbac.rs`, `schema.rs`, `prom.rs`, `problems.rs`, `certs.rs` | Logic without UI, unit-tested |
| `src/dev/` | Developer tools (feature `dev`): the off-screen snapshot harness and live smoke checks |

## How lists stay fast

Lists use the API server's **Table** format (`kubectl get`'s columns, computed server-side), then
are **watched**. A list of a namespaced kind in every namespace comes in up to three steps
(`list_fast` in `src/k8s/table.rs`):

1. Its first page (2,000 rows), shown at once.
2. When more follow, every namespace's rows without their objects, read from the API server's cache,
   32 namespaces at a time (`cells_by_namespace`): about 6 MB instead of 160 MB for 20,000 pods.
   These rows have no uid, labels or creation time yet.
3. The whole list with its objects' metadata, in one response from the cache, parsed as it streams
   in, with managedFields and the rest skipped (`list_whole`). It replaces the rows of step 2.

Rows keep only the metadata they need; full objects are fetched only when a custom column needs
them. Responses are gzip-compressed. The usage bars read the pods' requests and limits from a
separate lean list of the pods, read again every minute (or ten times as long as a read takes, when
that is longer). On a cluster with about 20,800 pods, the first rows show after 0.7 s and all of
them after 1.4 s ([measured](COMPARISON.md#measured)).

The Problems scan reads the big kinds from their cells too (`list_light`). What its checks need
of whole objects (Services, Ingresses and HTTP routes, the pods a Service selects, the workloads of
a namespace with an idle Service, external-secrets.io's objects) is parsed as each list streams in,
keeping only the fields the checks use; of the workloads, only those scaled to 0 are kept.

## Safety model

- **Read-only by default.** Write actions ask to enable write mode first.
- Every write is **confirmed**, shows the equivalent `kubectl` command, and most are validated with
  a **server dry run** first.
- **Protected contexts** require typing the context name.
- Actions your **RBAC** forbids are disabled (`SelfSubjectAccessReview`).
- **Agents** go through the hub: rules (deny, ask, allow), approvals, a rate limit, an audit log and
  a kill switch. Write mode and protected contexts don't apply to them: only their rules do.
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
  of their own.
- **Proxy passwords stay out of errors.** kube's error for a proxy it can't use prints the proxy's
  URL whole, user and password included. The client's build error puts that URL back without
  them, scheme or not (`user:password@host:1080` from `HTTPS_PROXY` reads as a URL to kube, not
  to a text), and leaves out the user and password of any other URL in its text, as of one in the
  login plugin's stderr line (`k8s::login`).
