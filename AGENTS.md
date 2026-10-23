# Notes for coding agents

## Building

- **Don't build unless you have to.** CI is free and runs clippy and the tests on every PR, so
  it is the check: run `cargo fmt --all` (no build), push, and read CI's log if it fails. Read
  the code instead of compiling to find out. Build locally only when CI's log is not enough,
  and then run the narrowest command (one test, or clippy alone).
- **Use at most half the machine's cores.** Set the job count on every cargo command
  that compiles (build, run, test, clippy, check, doc):

  ```bash
  CARGO_BUILD_JOBS=$(( $(nproc) / 2 )) cargo build --release --features dev
  ```

  A release build links with thin LTO in one codegen unit and takes several minutes;
  with every core busy the machine becomes unusable for the person working on it.
- **One build at a time.** Several agents or worktrees may share the machine. Before
  starting a build, check that no other is running (`pgrep -a cargo`) and wait for it to
  finish: two builds at half the cores each take all of them. Never get around a busy build
  with another `--profile`, `--target-dir` or `CARGO_TARGET_DIR`: a new one rebuilds every
  dependency (gpui, wgpu...) from scratch, in parallel with the others.
- Only the full run on `main` builds the docs, so a doc comment with a bare `<word>` (read as an
  HTML tag) breaks `main`, not the PR: put it in backticks.

## Pull requests

- One issue per PR, branched from `origin/main`, never stacked. The body says `Fixes #<n>.`
  with that issue's number: a wrong number closes the wrong issue when the PR merges.
- Confirm the bug on current `main` before fixing it; an audit report can be wrong or already
  fixed.
- Fix every copy of the bug. The same logic often lives twice (a page and its MCP tool, the
  form and the Apply view): search for the pattern before calling it done.
- Check the default case doesn't regress: one cluster, all namespaces accessible, no agent
  rules. A fix for the narrow case has broken the common one before.
- Merges are squash merges once `CI result` passes. Never merge your own PR before it is
  reviewed.
- `gh pr edit` fails on this repository (Projects classic); change a PR's title or body with
  `gh api -X PATCH repos/nahuel11500/recouvrance/pulls/<n> -f body=...`.

## Use the shared helpers

Each of these replaced several hand-written copies that disagreed. Use them instead of a new
copy; if one doesn't fit, extend it.

- **Async loads in a view: `crate::latest::Latest`.** One per kind of load. `spawn` /
  `spawn_in` replaces (and cancels) the running one, so a slow old result never lands over a
  newer one; `cancel` when the input goes away (a document with no kind, other pods).
- **Writes from the UI:** every request of the UI's clients passes `k8s::gate::WriteGate`,
  which refuses writes and shells while write mode is off, at the moment they are sent.
  `k8s::helm` does the same for install/upgrade/rollback/uninstall. Turn write mode on or off
  with `details::set_writes`, never by setting the `WriteMode` global alone. The agents' MCP
  server builds its clients with `connect_for_agents` (no gate: their rules decide).
- **Agent writes:** call `recheck` (or `stale_reason`) right before the cluster call. It
  decides again on the agent's current rules, so changes made while the call waited count.
- **A pod's requests and limits: `k8s::podspec`.** `of_value` / `LeanSpec::effective` with
  `Count::Scheduled` (what a node reserves: init containers count) or `Count::Running` (to
  set usage against). `podspec::total` adds them up over pods. It covers sidecars, init
  containers, overhead and pod-level resources; don't add up `spec.containers` by hand.
- **Which pods an object selects: `k8s::pod_selection`.** It handles a Service, a PDB (no
  selector is none, `{}` is all), a NetworkPolicy and workloads. An operator it doesn't know
  selects nothing.
- **Condition polarity: `status::true_is_bad` / `true_is_good`.** One list for every page,
  table and check.
- **Durations from kubectl columns: `k8s::age_seconds`.**
- **Deletes are pinned to the object picked:** pass its uid to `k8s::delete` /
  `delete_with`. A re-created object with the same name is left alone.
- **Loading config fails closed.** settings.json keeps every field that reads, and a context
  whose protection is unknown is protected. An unreadable agents.json is kept aside and agent
  access starts off. A rule without an `effect` denies. New config files should do the same:
  never let a read error turn into "allowed" or "not protected".

## Tests

- A `fix:` PR adds a test that fails without the fix. CI checks for one ("Fix has a test");
  the label `no test needed` skips it, for docs and CI changes.
- View behaviour over time (late results, reloads) can be tested with `#[gpui_kit::test]` and
  a `TestAppContext` (see `src/latest.rs`). Those tests need the `dev` feature:
  `#[cfg(all(test, feature = "dev"))]`.
- Rules that must hold for every input get a `proptest!`. The agent rule engine has them
  (`mcp::properties`), and so does the kubeconfig merge's context names
  (`k8s::config::name_properties`). Extend them when you change those.

## Bugs that keep coming back

Most fixed bugs were one of these. Check for them in new code.

- **Stale async results.** A task's result arrives after the user picked another chart, range,
  release or search. Keep the task handle so a new request replaces (and cancels) the old one,
  and check the result still matches the current choice before applying it. Don't reuse one
  `_task` slot for unrelated work: storing a new task drops, and cancels, the running one.
- **Several clusters.** Key objects by context, namespace and name, never by name alone. One
  cluster's error must not hide another's results (no `collect::<Result<Vec<_>>>()` across
  clusters), and a flag such as "forbidden" must not depend on which cluster answered last.
- **One failure taking everything down.** A down APIService, an add-on API group or one
  unreachable cluster should be skipped and reported, not fail discovery or the whole list.
  Don't hide errors with `unwrap_or_default()` either: say what failed.
- **API groups.** Check a kind with its real group (`networking.k8s.io` for NetworkPolicy, not
  `""`), and prefer the built-in kind when a CRD has the same short name.
- **Condition polarity.** For most conditions `True` is good, but for some (`ReplicaFailure`,
  `DisruptionTarget`, `MemoryPressure`...) `True` is bad. Don't color by `True`/`False` alone.
- **Rounding time.** `secs / 86400` rounds toward zero, so "expired 3 hours ago" reads as
  "0 days". Use `div_euclid`.
- **Streams of bytes.** Don't decode each read with `from_utf8_lossy`: a character can be split
  across two reads.
- **YAML documents.** Find document breaks with `yaml::doc_start` (it handles `--- # comment`),
  or `split_docs` in the Apply view, not a local check for an exact `---` line.
