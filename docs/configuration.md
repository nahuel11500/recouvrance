# Configuration

Recouvrance reads your kubeconfig like `kubectl` does and keeps its own settings in
`~/.config/recouvrance` (`$XDG_CONFIG_HOME/recouvrance`). Everything below can be changed in
**Settings**; the files are plain JSON or YAML if you prefer editing them.

## Files

| Path | What |
|---|---|
| `~/.config/recouvrance/settings.json` | Settings (below) |
| `~/.config/recouvrance/agents.json` | Agents, token hashes, rules (mode 0600) |
| `~/.config/recouvrance/agents.lock` | Held while the app or `serve` runs; `agent-create` refuses to run then |
| `~/.config/recouvrance/rules.yaml` | Agent rules as code (optional; wins over `agents.json` when present) |
| `~/.config/recouvrance/plugins.yaml` | Your commands on resources |
| `~/.config/recouvrance/kubeconfigs/*.yaml` | Kubeconfigs pasted in Settings (mode 0600) |
| `~/.config/recouvrance/templates/*.yaml` | Your templates for **Create / apply** |
| `~/.config/recouvrance/favorites.json`, `columns.json`, `custom-columns.json`, `column-widths.json` | Favorites and saved views (see [favorites.json](#favoritesjson)), column choices and widths |
| `~/.config/recouvrance/theme`, `zoom` | Light or dark choice (absent: follow the desktop), UI zoom (0.7 to 2.0) |
| `~/.config/recouvrance/anthropic-key` | The assistant's Anthropic API key, when saved in Settings (mode 0600; `ANTHROPIC_API_KEY` is used otherwise) |
| `~/.config/recouvrance/argocd-tokens.json` | Read-only Argo CD API tokens set in Settings → Per cluster, by context (mode 0600; see [Argo CD's diff](#argo-cds-diff-argocd_servers)) |
| `~/.config/recouvrance/argocd-api-tokens.json` | Argo CD API tokens set in Settings → Argo CD, by server address (mode 0600; see [Argo CD by its API](#argo-cd-by-its-api-argocd_api_servers)) |
| `~/.config/recouvrance/main-agent` | The built-in assistant's agent id and token (mode 0600) |
| `~/.local/share/recouvrance/audit.jsonl` | Agent activity log |
| `~/.local/share/recouvrance/timeline/<context>-<hash>.jsonl` | Changes recorded per context (mode 0600, see [Timeline](#timeline)) |

A JSON file that can't be read is kept aside as `<name>.bad-<time>` so the next save doesn't
overwrite it. In `settings.json`, only the keys that don't read get their default; the others,
such as `protection`, are kept; a `protection` entry that isn't `true` or `false` counts as
protected, a `delete_confirm_kinds` entry that doesn't read asks for the name, and a
`confirm_style` that doesn't read asks for names typed (so does every confirmation while
`settings.json` can't be read). When
`settings.json` doesn't read as an object at all, or its protection choices can't be read, every
context without a choice is protected, also after a restart, until you choose *Guess them again* in
Settings → Contexts. A `settings.json` that exists but can't be
read (say, owned by root after a run with `sudo`) is left alone: the app starts with the defaults,
counts every context not set as protected, saves nothing until it is restarted with a readable
file, and Settings says why. When `agents.json` can't be read, the app starts with no agents and
agent access off, and the Agents page says why.

## Kubeconfig

The contexts come from, merged in this order:

1. `KUBECONFIG` (several paths separated by `:`), or `~/.kube/config`;
2. files added in **Settings → Kubeconfig** (**Add files** or **Enter path**; `kubeconfigs`);
3. the files in a watched folder (**Watch folder**; `kubeconfig_folders`), oldest first: those
   without an extension or ending in `.yaml`, `.yml`, `.conf`, `.config` or `.kubeconfig`, hidden
   files (`.name`) left out;
4. kubeconfigs pasted in **Settings → Kubeconfig → Paste kubeconfig**, saved as they are in
   `~/.config/recouvrance/kubeconfigs/` (mode 0600). Before saving, Recouvrance lists the
   contexts and warns about commands run to log in (`exec`), files the kubeconfig reads and
   clusters that skip TLS verification. Removing one there deletes its file. Oldest first, so a
   new paste never takes a name an existing context has.

The page shows each source with its file and context counts, or why it can't be read. A file
that fails to parse is said by its path, line and column and a short reason (not valid YAML, a
missing field...), never by its lines around the error, where a token or a key can be; the
status line and the agents get the same when no file can be read. Changes to any of these files
are picked up within a few seconds. When two files define a context
(or cluster, or user) with the same name, the later one is renamed `name@file` so both stay usable.
A context name stays with the file that had it first (kept in `context_files`), even when files
are saved again, reordered, or one is briefly unreadable: protection and per-cluster settings
never move to another cluster. The name is freed when its file is deleted or no longer has it
(and unchanged for two seconds, so a file caught while `kubectl` rewrites it keeps its names). A
context already shown as `name@file` keeps that name when `name` is freed.

## settings.json

| Key | Default | What |
|---|---|---|
| `startup` | `"reconnect"` | Reconnect to the last contexts (`reconnect`) or open the context picker (`ask`) |
| `initial_view` | `"pods"` | First page after connecting: `pods`, `workloads`, `cluster`. A set of contexts with a saved session (`sessions`) reopens on its own page instead |
| `sessions` | | Remembered by the app: where each set of contexts was left, newest first, 20 kept: `{ "contexts": ["prod"], "tabs": [{ "page": "list", "kind": "deployments.apps", "namespaces": ["team-a"], "filter": "web" }], "active_tab": 0, "filters": { "pods": "api" } }`. `page` is `list`, `workloads`, `cluster`, `problems`, `timeline`, `helm` or `argo`; `kind` is the kubectl name. The first window reopens the session of the contexts it connects to; a connect where one of them fails (it doesn't answer, or its namespaces don't list) saves nothing over it, and the next connect to that set that reaches them all reopens it again. A value that doesn't read takes its default, an entry that doesn't read is dropped, and a type, namespace or page the clusters no longer have falls back to the default; none of it ever stops the file from loading |
| `window` | | Remembered by the app: `{ "x": 80, "y": 40, "width": 1500, "height": 920, "maximized": false }`, where the first window opens (centered when that is out of reach of every display). One that doesn't read opens at 1500×920, centered |
| `metrics_refresh`, `detail_refresh`, `overview_refresh` | 15, 5, 30 | Seconds between refreshes. `metrics_refresh` is the shortest wait between reads of metrics-server: the pods and nodes lists read it again when its next sample is due (up to twice its interval or 5 minutes later), and every `metrics_refresh` when its timestamps say nothing. Detail pages follow their object and its pods with watches; they read usage, charts and alerts every `metrics_refresh`, and what they link to (Services, revisions, events…) when the object or its pods change, or every 4 × `metrics_refresh`. `detail_refresh` is how often a detail page reads an object it can't watch (a role that only grants `get`). The cluster overview's refresh doesn't list the pods again: it lists them once, then follows them with a watch. A page that isn't on screen refreshes nothing: one in the back/forward history, or any page of a window hidden as the desktop reports it (minimized, on another workspace, or on macOS fully covered by other windows; Wayland compositors without the `suspended` state never report it). The cluster overview's watch of the pods' requests stops after 5 minutes off screen |
| `log_tail_lines`, `log_timestamps` | 1000, true | Log viewer |
| `kubeconfigs`, `kubeconfig_folders` | | Extra kubeconfig files and watched folders (see above) |
| `context_files` | | `{ "prod": "~/.kube/team-a.yaml" }`: the file each context name belongs to, so a name several files share stays with the first one, and the `name@file` names the others get stay with theirs (see above); filled in on its own |
| `context_colors` | | `{ "prod-eu": "#ef4444" }`: a colored strip per context |
| `protection` | guessed | `{ "prod-eu": true, "kind-dev": false }`: contexts where writes, shells and plugins need the context name typed (or a wait, see `confirm_style`). Contexts not listed are guessed from the context, user and cluster names and the server URL (`prod`, `prd`, `production`, `live`, `admin`, `root`); a context the kubeconfig doesn't have now, or any context while the kubeconfig can't be read, counts as protected. An older `protected_contexts` list is read once: its contexts become `true` |
| `protect_unset` | `false` | Set when the protection choices could not be read: contexts not listed in `protection` are protected instead of guessed. *Guess them again* in Settings → Contexts turns it off |
| `prometheus` | found automatically | `{ "ctx": "monitoring/prometheus:9090" }` (see below) |
| `clusters` | | Per-cluster settings (below) |
| `argocd_servers` | `{}` | `{ "argocd-hub": "https://argocd.example.com" }`: the Argo CD API server of a hub context, to read Argo CD's own diff from (see below). None: off. An entry that doesn't read is dropped. Settings → Per cluster |
| `argocd_api_servers` | `[]` | `["https://argocd.example.com"]`: Argo CD API servers read on the Argo CD page, besides the argocd CLI's (see below). Settings → Argo CD |
| `argocd_api_off` | `[]` | Argo CD API servers (their address) not read, whether set here or the argocd CLI's. Settings → Argo CD, the switch of each |
| `argocd_group_label` | `""` | `"app.kubernetes.io/part-of"`: the label whose value names an Application's service on the Argo CD page grouped by service. Empty: its name without the environment in it. Settings → Argo CD |
| `argocd_contexts` | `[]` | `["argocd-hub"]`: the contexts Argo CD's Applications are read from (see below). Empty: found on their own. A value that doesn't read is empty. Settings → Contexts, *Argo CD* |
| `extra_namespaces` | | Namespaces added by hand, per context |
| `metrics_enabled` | true | Read metrics-server and Prometheus at all |
| `accent` | `"brass"` | Accent color: `brass`, `harbour`, `sea`, `kelp`, `signal` or `violet` |
| `usage_range` | 60 | Range of the usage charts, in minutes (the last one chosen) |
| `saved_searches`, `filter_history` | | Table filters |
| `saved_forwards` | | Port-forwards to restore |
| `recent_forwards` | | Port-forwards started recently, offered again in the sidebar |
| `open_browser_on_forward` | true | Open the browser after a one-click port-forward on a web port |
| `assistant_backend` | `"auto"` | In-app assistant: `claude-code` (your Claude Code login), `api` (an Anthropic API key) or `auto` (Claude Code when installed, else the API key) |
| `assistant_model` | `"claude-sonnet-5-5"` | Model the assistant uses |
| `assistant_notes` | | The team's notes given to the assistant: where things run, dashboards, runbooks |
| `assistant_repos` | | Folders of the team's code the assistant may read (Claude Code only) |
| `delete_confirm` | `"ask"` | What deleting asks for kinds not in `delete_confirm_kinds`: `off` (delete right away), `ask` (a dialog) or `type` (the name typed) |
| `confirm_style` | `"wait"` | How deletes set to `type` and writes on protected contexts are confirmed: `wait` (the confirm button stays disabled for `confirm_wait_secs`, showing the seconds left, then works; nothing to type, and a dialog that would ask for several names waits once) or `type` (the names typed). A value that doesn't read asks for the names |
| `confirm_wait_secs` | `3` | Seconds the confirm button waits with `wait`, from 1 to 30; a value outside is brought within, one that isn't a whole number waits 30 |
| `delete_confirm_kinds` | by kind | `{ "Pod": "off", "Cluster.postgresql.cnpg.io": "type" }`: what deleting asks per kind, a custom resource with its group (see [features](features.md)). A value that doesn't read asks for the name |
| `namespace_visits` | | Remembered by the app: namespaces chosen per context, with a visit count and the last visit time. The namespace picker shows the five most frequent first (visits weigh half as much after a week without one); 30 kept per context. Older files' `recent_namespaces` are turned into visits once |
| `last_contexts`, `recent_contexts` | | Remembered by the app |
| `last_seen_version` | the version installed | Remembered by the app: the newest version whose release notes were shown (What's new, once after an update). A new install starts with its own version; a file without the key, from before What's new, shows the running version's notes once. Never set back by a downgrade. While `settings.json` can't be read, What's new doesn't show |
| `timeline` | | What the timeline records and keeps (see [Timeline](#timeline)) |

### Timeline

```json
"timeline": { "record": true, "kinds": ["configmaps", "deployments.apps"], "max_mb": 16, "days": 14 }
```

| Key | Default | What |
|---|---|---|
| `record` | `true` | Record changes to what the lists and the Problems scan watch. Off: nothing more is recorded; what was still shows |
| `kinds` | `[]` | Kinds watched whole while connected (kubectl names, `configmaps`, `secrets`, `deployments.apps`): each is a watch more on every connected cluster (in each namespace set in `clusters.<ctx>.namespaces` where there are some), and its records a diff of the whole object. Takes effect at the next connect. A name no connected cluster serves is said on the Timeline page |
| `max_mb` | `16` | MB of records kept per context, 1 to 1024. Past it, the oldest go: the file is rewritten down to 3/4 of it |
| `days` | `14` | Records older than this are dropped, 1 to 365 |

A `timeline` value that doesn't read gets these defaults (the other settings are kept).

The records of a context are in one file, `~/.local/share/recouvrance/timeline/<context>-<hash>.jsonl`
(`$XDG_DATA_HOME`): a header line naming the context, then a JSON line a change (time, kind,
namespace, name, uid, what changed, and the two versions compared, at most 64 KB each; a Secret's
values never). New records are appended in batches by a thread of their own, at most every half
second; the file is rewritten (atomically) only when records fall out of the bounds. A file that
doesn't read (cut short by a crash, edited by hand, another context's) is kept aside as
`<name>.bad-<time>` and recording starts over; the Timeline page says so.

### Metrics endpoint

Usage history is read from Prometheus or VictoriaMetrics through the API server's service proxy,
so it works wherever `kubectl` does. The service is found automatically; to choose one, set:

- `namespace/service:port`, e.g. `monitoring/prometheus-operated:9090`;
- with a path for VictoriaMetrics: `vm/vmselect:8481/select/0/prometheus` (cluster) or
  `vm/victoria-metrics:8428` (single node);
- or a URL reachable from your machine: `https://metrics.example.com/select/0/prometheus`.

Alerts are read from the same endpoint (`/api/v1/alerts`, every 30 seconds at most per cluster).
VictoriaMetrics answers there only when vmselect or vmsingle runs with `-vmalert.proxyURL`. Silences
come from an Alertmanager service found the same way (a service named `*alertmanager*`, port 9093);
without one, firing alerts show without silence information. An alert's **Rule** reads the alerting
rules (`/api/v1/rules?type=alert`, kept 5 minutes) and one range query for its chart, only when
clicked. With `metrics_enabled` off, none of these is read.

### Per cluster (`clusters`)

```json
"clusters": {
  "prod-eu": {
    "node_shell_image": "alpine:3.20",
    "namespaces": ["team-a", "team-b"],
    "metrics_disabled": true,
    "run_as": { "user": "super-admin", "groups": ["ops"] }
  }
}
```

`namespaces` is for users who may not list namespaces: they fill the namespace picker, and
lists show one of them at a time.

`node_shell_image` (`busybox:1.37` by default) runs node shells. When set, it also gives the
busybox a copy of a pod uses to read a stopped container's files (`busybox:1.37-musl` otherwise):
its `/bin/busybox` is copied into images that may have no libraries (distroless), so it must be
linked statically, as in the `-musl` and `-uclibc` busybox images (`busybox:1.37`, the glibc
build, and Alpine's are not).

`run_as` (Settings → Contexts, *Run as*; none by default) is who a write on this context may run
as, like `kubectl --as <user> --as-group <group>`: `user` is required, `groups` optional (a list).
It only adds a *Run as* checkbox to write confirmations, off each time (see
[features](features.md)); nothing runs as it otherwise. It fails closed: a value that
doesn't read (no user, `groups` not a list, a string instead of an object, characters that can't
go in an HTTP header) counts as none without costing the cluster's other settings, and while
`settings.json` can't be read no identity is offered. Your own role needs the `impersonate` verb
on that user and those groups.

### Argo CD hub (`argocd_contexts`)

The Argo CD panel on namespace and object pages, and the Argo CD page, read Applications from the cluster Argo CD runs
on (the hub). With `argocd_contexts` empty, the hub is found on its own: the connected clusters
first; only if none serves Applications, the kubeconfig's other contexts that can be asked without
a login prompt (no exec plugin, or the same exec command line as a connected context).

When the hub isn't connected and its kubeconfig user logs in with an exec plugin whose command
line differs from the connected contexts' (kubelogin with one OIDC client id per cluster, for
example), it is not asked on its own. The Argo CD page then lists those contexts, the ones named
like a hub first (`argo`, then `hub`, `meta`, `mgmt`, `platform`...): click the one Argo CD runs
on and it is asked (its login may run); if it serves Applications, it is kept here. Or tick
*Argo CD* on it in Settings → Contexts, or set it by hand:

```json
"argocd_contexts": ["eu-platform-meta"]
```

Only the named contexts are asked then (their login may run). Connecting the hub as well works too.
Agents read Argo CD (`argocd_apps`, and the Application named by `describe_resource` and
`get_changes`) on these contexts too, else on the hubs already found, among the contexts their
rules name (see [agents](agents.md)).

### Argo CD's diff (`argocd_servers`)

An Application's page can show what a sync would change, as Argo CD renders it from git: set the
Argo CD API server of the hub context in Settings → Per cluster (`https://argocd.example.com`,
with a path when Argo CD is served under one). Plain `http://` is only taken for this machine (a
port-forward to argocd-server), so a token never crosses the network in clear.

*Show Argo CD's diff* then sends one request, `GET /api/v1/applications/{name}/managed-resources`,
to that server only, and only on that click. It logs in with, in this order:

- the token set in Settings → Per cluster (kept in `argocd-tokens.json`, mode 0600, never shown
  again; *Forget* removes it). A project role or local account limited to `applications, get` is
  enough;
- the argocd CLI's login to that server: the token `argocd login` saved in its config file
  (`$ARGOCD_CONFIG_DIR/config`, else `~/.argocd/config` when that folder exists, else
  `~/.config/argocd/config`). Recouvrance only reads it; when it has expired, run
  `argocd login <server> --sso` again. The CLI's `--core` and port-forward modes have no address
  to ask.

Errors never show the token. Secret values are masked by Recouvrance itself, whatever Argo CD
sends (its own masking leaked predicted Secret states before 3.2.12, 3.3.10 and 3.4.2).

### Argo CD by its API (`argocd_api_servers`)

The Argo CD page also reads Argo CD by its own API, for when you may use Argo CD but not the
cluster it runs on. Its servers are:

- those the argocd CLI is logged in to: each context of its config file (found as above) with a
  token, one per server, the current context's first. Its `--insecure` (the certificate is not
  checked), `--plaintext` (taken only for this machine, as above) and `--grpc-web-root-path`
  (Argo CD served under a path) are honoured; `--grpc-web` makes no difference to the REST API;
  a `--core` context has no server;
- those added in Settings → Argo CD: an address, and a token kept in `argocd-api-tokens.json`
  (mode 0600, never shown again; *Forget token* removes it). A token set here wins over the CLI's
  for the same server.

A config that can't be read fails closed: a CLI config that doesn't parse gives no server, and a
tokens file that doesn't read gives no token at all (not even the CLI's), each said in Settings →
Argo CD and above the list. An expired token is said before anything is sent. Errors never show a
token, and a token is only sent to its server, over https, without following redirects.

Each server's application stream (`/api/v1/stream/applications`, server-sent events, with
`fields` so only what the page shows is sent) is opened first: Argo CD subscribes it to every
change before anything else. Then the Applications are listed (`GET /api/v1/applications`, with
`fields`), and what the stream sent meanwhile is applied over the list in order, the newer of the
two winning (by resource version) and a deletion removing: nothing changed between the list and
the stream is lost. A stream that ends, breaks or stays silent 15 minutes is opened again after a
backoff of 1 s doubling to 60 s (back to 1 s once one stayed open a minute), listing again; that
is routine and not shown as an error, while a list or a stream that can't be opened is. A refused
login stops until the login or the config changes: a new `argocd login`, or a token set in
Settings, is noticed within a second while the page is shown. The page holds the feeds while a
window shows it, shared by every window, and lets them go when none does. Only the fields shown
are kept, so memory stays small however many Applications there are. The projects' sync windows
are read beside the stream, and again every 5 minutes: one `GET /api/v1/projects`, or, when they
can't be listed, `GET /api/v1/projects/{name}` (its `spec.syncWindows`) for each project of the
Applications, up to 50. Reading needs `applications, get` and `projects, get`; the row menu's
actions need what Argo CD's own UI would (`applications, sync` and so on). An action is sent with
the server's login as the config has it then, and not at all to a server removed or turned off
since the list was drawn. When settings.json can't be read, every server is off (which were
turned off is unknown).

An Application read by the API and by its hub is the same one when the hub's Argo CD server set
in Settings → Per cluster (`argocd_servers`) is that API server, and its namespace and name are
the same: it shows once, as read on the hub (whose page it opens). Without that setting, the two
are kept apart.

## favorites.json

Every favorite and saved view, in one file the app writes when a star changes. Every window
shares the same favorites, so one never saves over another's:

```json
{
  "contexts": ["prod"],
  "namespaces": [{"context": "prod", "namespace": "shop"}],
  "applications": [
    {"argocd": "context:hub", "namespace": "argocd", "name": "shop-prod", "context": "hub"},
    {"argocd": "https://argocd.example.com", "namespace": "argocd", "name": "shop-dev"}
  ],
  "objects": [{"context": "prod", "kind": "deployments.apps", "namespace": "shop", "name": "api"}],
  "kinds": ["pods", "certificates.cert-manager.io"],
  "views": [{"name": "Shop pods", "kind": "pods", "namespaces": ["shop"], "filter": "!Running"}]
}
```

| Key | Keyed by | What |
|---|---|---|
| `contexts` | context name | Favorite contexts, first in the context picker |
| `namespaces` | context + namespace | Favorite namespaces, first in the namespace picker while their context is connected |
| `applications` | Argo CD instance + namespace + name | Favorite Argo CD Applications |
| `objects` | context + kind (kubectl name, `plural.group`) + namespace (`null` for cluster-scoped) + name | Favorite objects |
| `kinds` | kubectl name | Starred resource types in the sidebar |
| `views` | name | Saved table views |

An Application's `argocd` is its Argo CD instance, as the Argo CD page keys it: the Argo CD
server's address (scheme and host in lowercase, without a default port) when Argo CD is read
through its API or its hub context has its server set (Settings → Per cluster), else
`context:<hub context>`. `context` is the hub context it was starred on, when read from one:
`context:<context>` is another name of the same instance, and its page opens on that context.
An Application is one favorite through any of these names: starred through one, it shows as
starred and is unstarred through the other, every entry naming it removed. One known only by an
API server (no `context`) opens the Argo CD page. An Application starred from its page or a table
of Applications is kept here, not under `objects`; one an older version kept under `objects`
moves here when the file is read.

Loading keeps every entry that reads and drops the others, one by one: an entry of the wrong
shape, with a field missing or empty, or a list that isn't one costs only itself; an entry listed
twice is kept once (a view, by its first name). When an entry was dropped, the file as it was is
copied to `favorites.json.bad-<time>` before the first save writes it without them. Keys the app
doesn't know (a newer version's) are written back as they were. A file that isn't a JSON object
is kept aside as `favorites.json.bad-<time>`, and the app starts with no favorites. A file that
exists but can't be read (permissions, say) is left alone: the app starts with no favorites and
saves none until it is restarted with a readable file. A favorite whose context is gone from the
kubeconfig stays in the file and the sidebar says so; remove it there with its cross.

## rules.yaml

Agent rules as code, in the spirit of Claude Code's allow/ask/deny lists:

```yaml
version: 2
agents:
  Claude:
    allow:
      - get,list,logs *                            # read everything
      - restart,scale deployments/web-* -n team-*  # names after the slash
      - refresh applications.argoproj.io           # Argo CD refresh, deploys nothing
    ask:
      - patch,sync,apply,delete * --context staging
      - patch,sync,suspend,apply,delete * --context @protected
    deny:
      - "* secrets"
      - "* * --context *prod*"
      - exec pods until 2026-11-01                 # expires
    limits: { calls_per_minute: 120 }
```

`limits` takes `calls_per_minute` (default 120, 0 for no limit), `max_log_lines` and `max_rows`
(default 500 each: the most log lines and list rows one call returns) and `exec_timeout` (seconds
an exec command may run, default 30).

A rule is `VERBS RESOURCES[/NAMES] [-n NAMESPACES] [--context CONTEXTS] [until TIME]`. Lists are
comma-separated, `*` globs work everywhere, an omitted part means `*`, and `-` is an empty list
(the rule matches nothing; an exported rule with nothing chosen yet is written that way). The verbs
are `get list logs reveal exec scale restart patch refresh sync suspend apply delete drain forward`
(or `*`); `refresh`, `sync` and `suspend` are the GitOps actions (see [agents](agents.md#rules)). In
ask and deny rules, the context `@protected` stands for the contexts protected in Settings →
Contexts, read at each call (every context when the protection can't be read); it never makes a
request possible that no other rule covers, and an allow or approved rule naming it fails the load.
Deny wins, then ask, then allow; anything no rule matches is denied. `approved` lists the requests you approved with
*Always allow this* or *Allow for 1h*: they are allowed even where an ask rule matches, never where a
deny rule does. An `approved` rule's resource is matched as written (`pods` is not
`pods.metrics.k8s.io`), and its names and contexts case included. The file is reloaded when it changes, and edits on the Agents page are written back
to it (its comments are not kept). An edit on the Agents page made after you saved the file, but
before it was reloaded, is refused so it doesn't overwrite yours. A key the file doesn't know (a
misspelt `deny:`) fails the load, like a rule that doesn't parse: the rules in force stay as they
were and the Agents page shows the error.

While the file exists, an agent it doesn't name has no rules, so everything it asks is denied:
removing an agent's section takes its access away. Deleting an agent on the Agents page removes its
section too. Before 0.3.0 a new agent got no section until its rules were edited: on the first start
of 0.3.0 such an agent keeps its rules, and its section is written to the file. Before the GitOps
verbs, GitOps actions were checked as `patch`. `version: 2` at the top says the file's rules use
them. A file without it (written before them, restored from git, copied from another machine, or
given with no agents.json) is read as written before them, whatever agents.json says: every rule
that names `patch` gets `refresh`, `sync` and `suspend` when it is loaded, and the file is written
again with `version: 2`, once (even when no rule changed). A verb you take out of a rule after that
stays out. A file without `version` whose rules name `refresh`, `sync` or `suspend` is not loaded
(the rules in force stay, and the Agents page says to add `version: 2` at the top): no rule could
name them before, so it was written for them, and upgrading it would give back a verb taken out.
Write `version: 2` in a file you write by hand, or its `patch` rules cover GitOps actions too. A
file with a higher `version`, written by a newer Recouvrance, is not loaded either. If the file
changed on disk meanwhile, it is not written over: the load fails with the reason, and your edit is
loaded (and upgraded, if it has no `version` either) next.
If the folder of the file can't be written (managed by another tool), its rules are still loaded,
upgraded the same way, and the Agents page says why the file wasn't written; while it has no
`version`, each load upgrades it again, until it is written. Edits on the Agents page try to write
it too, and fail with the reason rather than being undone at the next load.

Each section is for the agent with that name, so agent names are unique: creating a second agent
with a taken name is refused, and if two agents already share a name the file is neither loaded nor
written until you delete one and save the file again.

## plugins.yaml

```yaml
plugins:
  - name: Tail with stern
    kinds: [deployments, statefulsets]
    command: stern -n $NAMESPACE $NAME
    read_only: true
  - name: Open in Grafana
    kinds: [pods]
    # xdg-open on Linux; on macOS, open
    command: xdg-open "https://grafana.example/d/pods?var-pod=$RECOUVRANCE_NAME"
    mode: background
    read_only: true
  - name: Rolling restart
    description: Restart every pod, one at a time
    kinds: [statefulsets]
    command: kubectl rollout restart statefulset -n $NAMESPACE $NAME
    confirm: true
```

Fields: `name` and `command` (required); `kinds`, kubectl names such as `pods` or
`certificates.cert-manager.io`, or `*` for all; `mode`, `terminal` (the default) or `background`;
`description`, shown as the button's tooltip instead of the command; `confirm: true` to ask before
running; `read_only: true` for plugins that only read.

The command runs with `sh -c`. The object is passed in environment variables, never pasted into
the command: `RECOUVRANCE_CONTEXT`, `RECOUVRANCE_NAMESPACE`, `RECOUVRANCE_NAME` and `RECOUVRANCE_KIND`.
The shorthands `$CONTEXT`, `$NAMESPACE`, `$NAME` and `$KIND` still work: each becomes a reference
to its variable, quoted for where it stands (bare, in double quotes or in single quotes). `KUBECONFIG` points at a private copy with the context selected, so `kubectl` just works. Plugins run in a
terminal page, or in the background with the last line of output shown.

A plugin may change the cluster, so it is treated as a write: it needs write mode, and on a
protected context the context name typed (or a wait, see `confirm_style`). A plugin that only reads declares `read_only: true` and
runs in read-only mode too. `confirm: true` always asks before running. A plugin that writes runs
again only from its button, with these checks, not from its terminal page.

## Templates

Any YAML file in `~/.config/recouvrance/templates/` shows up under **Create / apply → Templates**,
next to the built-in ones. `${NAMESPACE}` is replaced by the namespace you are in, wherever it
appears (for example `namespace: ${NAMESPACE}` or `value: ${NAMESPACE}-cache`). For templates
written before this placeholder, a bare `NAMESPACE` is still replaced when it is the whole value of
a `namespace:` key; elsewhere (such as `POD_NAMESPACE`) it is left as it is.
