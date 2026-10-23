# Assistant and agents (MCP)

## Assistant

*Assistant* in the sidebar is a chat with Claude about your clusters. It runs through your local
Claude Code (your subscription, no API key); an Anthropic API key works too if you prefer. It uses
the same agent as your Claude Code connection (below), so one set of rules, approvals and activity
log covers both. **Stop** ends a question that is still running; with an API key, a request that
gets no answer for 10 minutes fails.

## Agents (MCP)

While the app runs, it serves MCP on `http://127.0.0.1:47070/mcp` (`recouvrance serve` runs it
without the window; there nobody can approve, so a call an *Ask me* rule covers is refused at once
with "needs approval in the Recouvrance window": run the app, or add an Allow rule). A list where
some rows ask and others are allowed (some namespaces, or some names) is not refused: when it is not
approved (or cannot be), it shows only the allowed rows, and a rejection is still logged. Approving
it shows the rows of every Ask rule the list meets. In **Agents**, click **Connect Claude Code**: it registers the server in your
Claude Code (`claude mcp add --scope user recouvrance …`). There is one agent, "Claude", with its rules;
*Several agents…* gives other tools their own token and rules if you ever need that. Tokens are
stored hashed (the assistant's copy is in a 0600 file).

## Rules

Each agent has rules, like Kubernetes RBAC with an effect:

| Field | Example |
|---|---|
| Effect | Allow · Ask me · Deny |
| Verbs | get, list, logs, reveal (Secret values), exec, scale, restart, patch, apply, delete, * |
| Resources | `pods`, `deployments`, `*.cert-manager.io`, `*` |
| Namespaces | `team-a`, `team-*`, `*` (also covers cluster-scoped objects) |
| Contexts | `eu-*`, `staging` |
| Names | `web-*`, `*` |

**Deny wins, then Ask me, then Allow; anything no rule matches is denied.** A Deny limited to some
names hides those objects from lists rather than refusing the whole list. Presets get you
started, rules can expire (e.g. "Allow for 1h" on an approval), a per-agent rate limit caps calls
per minute, *Test access* shows the verdict for any request and which rule decided, and approval
banners show the request with its key arguments (command, the whole patch, replicas, action, dry
run) and offer **Always allow this** to add a narrow rule; it wins over the Ask rule that asked, so
the same request is not asked again (until you edit the rule). For `apply_manifest` the banner lists
every object (kind, namespace/name) and the YAML that will be applied, a Secret's values masked; the
activity log keeps each object with a hash of that YAML, and the objects already applied when a call
is cut off.
The *Always allow this* and *Allow for 1h* offers are left out where no rule can be that narrow:
exec (a rule cannot pin the command), patches and GitOps actions (nor the patch), scale (nor the
replica count), `apply_manifest` (nor the content), writes with no object name, and dry runs (the
rule would allow the real write too).

Before any rule is checked, a call's context must be named by one of the agent's Allow or Ask rules
(other contexts are never contacted), and the names it passes are validated: a namespace or
container must be a DNS label, an object name may hold only letters, digits and `- . _ : @ +` (no
`/`, `?`, `%`, `..` or spaces). An invalid name fails the call with a message saying why. What the
API server returns must be the object asked for, and a Secret's values are masked whatever kind
was asked for. `get_logs` returns at most 1 MiB. `drain_node` is refused, without cordoning, when
the node runs pods it would evict in a namespace a Deny rule covers for `delete` on pods.

Tools: `my_permissions`, `list_contexts`, `list_resource_types`, `list_resources`, `get_resource`,
`describe_resource`, `top_pods`, `search`, `get_events`, `find_problems`, `check_certificates`, `usage_history`, `drain_node`, `gitops`,
`get_logs`, `exec_command`, `scale`, `rollout_restart`, `patch_resource`, `apply_manifest`,
`delete_resource` — writes support `dry_run`.

Prompts (slash commands in Claude Code: `/mcp__recouvrance__troubleshoot` …): `troubleshoot`, `rightsize`,
`cert-audit`, `cluster-health`. They chain the tools above into a read-only workflow.

Every call is logged with its decision and rule, and appended to
`~/.local/share/recouvrance/audit.jsonl` (the folder is 0700, the file 0600). At 10 MiB the log moves to
`audit.jsonl.1`, replacing the older one. If the log cannot be written, the Activity page says so. A
kill switch cuts all agent access at once.

The Activity page groups calls by task: one agent session, split where the agent paused for more than
10 minutes. Each group says what it touched (contexts, namespaces, objects); repeated calls of a tool on
the same target fold into one line, and any line opens to its full arguments and result. Filter by
writes, denied or failed calls, context, agent or text. It shows the latest 300 calls; "Show older"
goes further back, and then reads the log on disk.

> These rules govern what agents do *through Recouvrance*. An agent with shell access can still run
> `kubectl` with your kubeconfig, so block `kubectl`/`helm` in its own permissions if Recouvrance
> should be the only way in.

Rules can also be kept as code in `~/.config/recouvrance/rules.yaml` (Agents → Rules as code); see
[configuration](configuration.md#rulesyaml).
