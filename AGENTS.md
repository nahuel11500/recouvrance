# Notes for coding agents

## Building

- **Use at most half the machine's cores.** Set the job count on every cargo command
  that compiles (build, run, test, clippy, check, doc):

  ```bash
  CARGO_BUILD_JOBS=$(( $(nproc) / 2 )) cargo build --release --features dev
  ```

  A release build links with thin LTO in one codegen unit and takes several minutes;
  with every core busy the machine becomes unusable for the person working on it.
- **One build at a time.** Several agents or worktrees may share the machine. Before
  starting a build, check that no other is running (`pgrep -a cargo`) and wait for it to
  finish: two builds at half the cores each take all of them.
- Before a PR: `cargo fmt`, `cargo clippy --all-targets --features dev -- -D warnings`,
  `cargo test --features dev` and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
  --document-private-items`, all with the job count above. Only the full run on `main` builds
  the docs, so a doc comment with a bare `<word>` (read as an HTML tag) breaks `main`, not the
  PR: put it in backticks.
- A target directory shared between worktrees can report a stale pass. Check you are on your
  branch before trusting a green run; CI on the PR is the final word.

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
