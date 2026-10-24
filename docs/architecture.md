# Architecture

Recouvrance is one Rust binary: a [gpui](https://github.com/zed-industries/zed) window (through
`gpui-kit`) on top of [kube-rs](https://kube.rs), plus a local MCP server ([rmcp](https://github.com/modelcontextprotocol/rust-sdk)).

## Threads

- The **UI thread** runs gpui: views are `Entity`s, rendered every frame they change.
- Kubernetes calls run on a **tokio runtime** (`k8s::run`, `k8s::spawn_guarded`). Long-lived work
  (watches, log streams, port-forwards) returns a `TaskGuard`: dropping it cancels the work, so a
  page that goes away stops its streams.
- Pages hidden in the back/forward history pause their polling (`app::wait_visible`).

## Source map

| Path | What |
|---|---|
| `src/main.rs` | Entry point: the window, `serve` (headless MCP), `agent-create` |
| `src/app/` | The main window: contexts, sidebar, resource list page, tabs and history, theme |
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

Lists use the API server's **Table** format (`kubectl get`'s columns, computed server-side), are
paged, then **watched**. Rows keep only the metadata they need; full objects are fetched only when a
custom column needs them. Responses are gzip-compressed. This keeps very large clusters (tens of
thousands of pods) in a few hundred megabytes.

## Safety model

- **Read-only by default.** Write actions ask to enable write mode first.
- Every write is **confirmed**, shows the equivalent `kubectl` command, and most are validated with
  a **server dry run** first.
- **Protected contexts** require typing the context name.
- Actions your **RBAC** forbids are disabled (`SelfSubjectAccessReview`).
- **Agents** go through the hub: rules (deny, ask, allow), approvals, a rate limit, an audit log and
  a kill switch. Write mode and protected contexts don't apply to them: only their rules do.
