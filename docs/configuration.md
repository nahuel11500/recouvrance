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
| `~/.config/recouvrance/favorites.json`, `columns.json`, `custom-columns.json`, `column-widths.json` | Favorites, saved views, column choices and widths |
| `~/.config/recouvrance/theme`, `zoom` | Light or dark choice (absent: follow the desktop), UI zoom (0.7 to 2.0) |
| `~/.config/recouvrance/anthropic-key` | The assistant's Anthropic API key, when saved in Settings (mode 0600; `ANTHROPIC_API_KEY` is used otherwise) |
| `~/.config/recouvrance/main-agent` | The built-in assistant's agent id and token (mode 0600) |
| `~/.local/share/recouvrance/audit.jsonl` | Agent activity log |

A JSON file that can't be read is kept aside as `<name>.bad-<time>` so the next save doesn't
overwrite it. In `settings.json`, only the keys that don't read get their default; the others,
such as `protection`, are kept; a `protection` entry that isn't `true` or `false` counts as
protected, and a `delete_confirm_kinds` entry that doesn't read asks for the name. When
`settings.json` doesn't read as an object at all, or its protection choices can't be read, every
context without a choice is protected for that session. A `settings.json` that exists but can't be
read (say, owned by root after a run with `sudo`) is left alone: the app starts with the defaults,
counts every context not set as protected, saves nothing until it is restarted with a readable
file, and Settings says why. When `agents.json` can't be read, the app starts with no agents and
agent access off, and the Agents page says why.

## Kubeconfig

The contexts come from, merged in this order:

1. `KUBECONFIG` (several paths separated by `:`), or `~/.kube/config`;
2. files added in **Settings → Kubeconfig** (**Add files** or **Enter path**; `kubeconfigs`);
3. every file in a watched folder (**Watch folder**; `kubeconfig_folders`), oldest first;
4. kubeconfigs pasted in **Settings → Kubeconfig → Paste kubeconfig**, saved as they are in
   `~/.config/recouvrance/kubeconfigs/` (mode 0600). Before saving, Recouvrance lists the
   contexts and warns about commands run to log in (`exec`), files the kubeconfig reads and
   clusters that skip TLS verification. Removing one there deletes its file. Oldest first, so a
   new paste never takes a name an existing context has.

The page shows each source with its file and context counts, or why it can't be read. Changes
to any of these files are picked up within a few seconds. When two files define a context
(or cluster, or user) with the same name, the later one is renamed `name@file` so both stay usable.
A context name stays with the file that had it first (kept in `context_files`), even when files
are saved again, reordered, or one is briefly unreadable: protection and per-cluster settings
never move to another cluster. The name is freed when its file is deleted or no longer has it
(read twice the same way, so a file caught while `kubectl` rewrites it keeps its names). A
context already shown as `name@file` keeps that name when `name` is freed.

## settings.json

| Key | Default | What |
|---|---|---|
| `startup` | `"reconnect"` | Reconnect to the last contexts (`reconnect`) or open the context picker (`ask`) |
| `initial_view` | `"pods"` | First page after connecting: `pods`, `workloads`, `cluster` |
| `metrics_refresh`, `detail_refresh`, `overview_refresh` | 15, 5, 30 | Seconds between refreshes |
| `log_tail_lines`, `log_timestamps` | 1000, true | Log viewer |
| `kubeconfigs`, `kubeconfig_folders` | | Extra kubeconfig files and watched folders (see above) |
| `context_files` | | `{ "prod": "~/.kube/team-a.yaml" }`: the file each context name belongs to, so a name several files share stays with the first one, and the `name@file` names the others get stay with theirs (see above); filled in on its own |
| `context_colors` | | `{ "prod-eu": "#ef4444" }`: a colored strip per context |
| `protection` | guessed | `{ "prod-eu": true, "kind-dev": false }`: contexts where writes, shells and plugins need the context name typed. Contexts not listed are guessed from the context, user and cluster names and the server URL (`prod`, `prd`, `production`, `live`, `admin`, `root`); a context the kubeconfig doesn't have now, or any context while the kubeconfig can't be read, counts as protected. An older `protected_contexts` list is read once: its contexts become `true` |
| `prometheus` | found automatically | `{ "ctx": "monitoring/prometheus:9090" }` (see below) |
| `clusters` | | Per-cluster settings (below) |
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
| `namespace_visits` | | Remembered by the app: namespaces chosen per context, with a visit count and the last visit time. The namespace picker shows the five most frequent first (visits weigh half as much after a week without one); 30 kept per context. Older files' `recent_namespaces` are turned into visits once |
| `last_contexts`, `recent_contexts` | | Remembered by the app |

### Metrics endpoint

Usage history is read from Prometheus or VictoriaMetrics through the API server's service proxy,
so it works wherever `kubectl` does. The service is found automatically; to choose one, set:

- `namespace/service:port`, e.g. `monitoring/prometheus-operated:9090`;
- with a path for VictoriaMetrics: `vm/vmselect:8481/select/0/prometheus` (cluster) or
  `vm/victoria-metrics:8428` (single node);
- or a URL reachable from your machine: `https://metrics.example.com/select/0/prometheus`.

### Per cluster (`clusters`)

```json
"clusters": {
  "prod-eu": {
    "node_shell_image": "alpine:3.20",
    "namespaces": ["team-a", "team-b"],
    "metrics_disabled": true
  }
}
```

`namespaces` is for users who may not list namespaces: they fill the namespace picker, and
lists show one of them at a time.

## rules.yaml

Agent rules as code, in the spirit of Claude Code's allow/ask/deny lists:

```yaml
agents:
  Claude:
    allow:
      - get,list,logs *                            # read everything
      - restart,scale deployments/web-* -n team-*  # names after the slash
    ask:
      - patch,apply,delete * --context staging
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
(the rule matches nothing; an exported rule with nothing chosen yet is written that way). Deny wins, then ask,
then allow; anything no rule matches is denied. `approved` lists the requests you approved with
*Always allow this* or *Allow for 1h*: they are allowed even where an ask rule matches, never where a
deny rule does. An `approved` rule's resource is matched as written (`pods` is not
`pods.metrics.k8s.io`), and its names and contexts case included. The file is reloaded when it changes, and edits on the Agents page are written back
to it (its comments are not kept). An edit on the Agents page made after you saved the file, but
before it was reloaded, is refused so it doesn't overwrite yours. A key the file doesn't know (a
misspelt `deny:`) fails the load, like a rule that doesn't parse: the rules in force stay as they
were and the Agents page shows the error.

While the file exists, an agent it doesn't name has no rules, so everything it asks is denied:
removing an agent's section takes its access away. Deleting an agent on the Agents page removes its
section too.

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
protected context the context name typed. A plugin that only reads declares `read_only: true` and
runs in read-only mode too. `confirm: true` always asks before running. A plugin that writes runs
again only from its button, with these checks, not from its terminal page.

## Templates

Any YAML file in `~/.config/recouvrance/templates/` shows up under **Create / apply → Templates**,
next to the built-in ones. `${NAMESPACE}` is replaced by the namespace you are in, wherever it
appears (for example `namespace: ${NAMESPACE}` or `value: ${NAMESPACE}-cache`). For templates
written before this placeholder, a bare `NAMESPACE` is still replaced when it is the whole value of
a `namespace:` key; elsewhere (such as `POD_NAMESPACE`) it is left as it is.
