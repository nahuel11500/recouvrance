#!/usr/bin/env bash
# The speed part of the release check (AGENTS.md, "Before a release"): the last release's dev
# build against this checkout's, on one cluster, read only, printed as the table to post on the
# release PR.
#
#   scripts/release-bench.sh <last-tag> <context> [scratch-dir]
#
# Builds: each is a release build with `--features dev`, kept in
# ${XDG_CACHE_HOME:-~/.cache}/recouvrance-bench/<name>/recouvrance, so the next check reuses it:
# the last tag's under its tag (built once, from a worktree of the tag), this checkout's under
# v<its Cargo.toml version> when that version has no tag yet (the release PR: it is the next
# check's "before"), else under `head`. A kept build is reused while its source tree is the
# same. Builds use the main checkout's target dir, one at a time (AGENTS.md, Building).
#
# Runs, each instance with its own scratch XDG folders and MCP port (never 47070):
# - timings, the builds alternated over two rounds (before, after, then after, before): the
#   pods list (RECOUVRANCE_SMOKE), the Problems scan twice (RECOUVRANCE_PROBLEMS=twice), the
#   Workloads overview (RECOUVRANCE_DASH=1), going back to a list
#   (RECOUVRANCE_BACKTIME=pods,deployments,2), the first rows after a connect with one context
#   and with a context that can't be reached (RECOUVRANCE_FIRSTROWS=1), the whole app's memory
#   with the pods list on screen (RECOUVRANCE_METER=1);
# - two find_problems calls 60 s apart through the agents' MCP server, one build after the other;
# - the Problems page on screen for BENCH_MINUTES (10) with one window, then two
#   (RECOUVRANCE_WINDOWS=2, on builds that have it), every build at once: each process counts
#   its own requests, so they take 10 minutes, not 40.
#
# Read only: write mode stays off, and the agent made for find_problems may only get and list.
#
# BENCH_PARTS picks the parts (default: all of them):
#   smoke problems dash back first unreachable memory find_problems rate
# BENCH_MINUTES: how long the request-rate runs last (default 10).
# BENCH_ROUNDS: how many rounds of timings (default 2).
# BENCH_BEFORE, BENCH_AFTER: a dev build to measure instead of the last tag's or this
# checkout's (the tag with a hook cherry-picked onto it, say).
set -euo pipefail

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
    exit "${1:-0}"
}
[ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] && usage
[ $# -ge 2 ] || usage 1

tag=$1
context=$2
out=${3:-$(mktemp -d -t recouvrance-bench.XXXXXX)}
parts=${BENCH_PARTS:-smoke problems dash back first unreachable memory find_problems rate}
minutes=${BENCH_MINUTES:-10}
rounds=${BENCH_ROUNDS:-2}
cache=${XDG_CACHE_HOME:-$HOME/.cache}/recouvrance-bench
repo=$(git rev-parse --show-toplevel)
main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
kubeconfig=${KUBECONFIG:-$HOME/.kube/config}

want() { [[ " $parts " == *" $1 "* ]]; }
say() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

mkdir -p "$out/logs" "$cache"
git -C "$repo" rev-parse --verify --quiet "$tag^{commit}" >/dev/null || {
    echo "release-bench: no tag $tag (git fetch --tags?)" >&2
    exit 1
}

# --- Builds ---------------------------------------------------------------------------------

# Build the checkout at $1 into $2/recouvrance, one build at a time on this machine.
build() {
    local src=$1 dest=$2 target="$main/target"
    mkdir -p "$target" "$dest"
    exec 8>"$target/.pre-push.lock"
    flock 8
    while pgrep -x cargo >/dev/null; do
        say "waiting for another cargo to finish"
        sleep 20
    done
    say "building $src (release, --features dev)"
    # The target dir is shared with other checkouts: a source file older than the last build
    # there would pass for built, so the crate is always compiled again.
    touch "$src/src/main.rs"
    (cd "$src" && CARGO_TARGET_DIR="$target" CARGO_BUILD_JOBS=$(($(nproc) / 2)) \
        cargo build --locked --release --features dev)
    cp "$target/release/recouvrance" "$dest/recouvrance.new"
    mv "$dest/recouvrance.new" "$dest/recouvrance"
    exec 8>&-
}

# Whether the kept build in $1 was made from the tree of commit $2.
kept() {
    [ -x "$1/recouvrance" ] && [ -f "$1/tree" ] && [ "$(cat "$1/tree")" = "$(git -C "$repo" rev-parse "$2^{tree}")" ]
}

before_dir="$cache/$tag"
if [ -n "${BENCH_BEFORE:-}" ]; then
    before=$BENCH_BEFORE
elif ! kept "$before_dir" "$tag"; then
    wt=$(mktemp -d -t recouvrance-bench-src.XXXXXX)
    git -C "$repo" worktree add --detach --quiet "$wt" "$tag"
    build "$wt" "$before_dir"
    git -C "$repo" worktree remove --force "$wt"
    git -C "$repo" rev-parse "$tag^{tree}" >"$before_dir/tree"
fi
before=${before:-$before_dir/recouvrance}

if [ -n "${BENCH_AFTER:-}" ]; then
    after=$BENCH_AFTER
    after_name=$(basename "$after")
else
    [ -z "$(git -C "$repo" status --porcelain --untracked-files=no)" ] || {
        echo "release-bench: commit or stash the changes first: the build is kept by commit" >&2
        exit 1
    }
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)
    after_name=head
    git -C "$repo" rev-parse --verify --quiet "v$version^{commit}" >/dev/null || after_name="v$version"
    after_dir="$cache/$after_name"
    if ! kept "$after_dir" HEAD; then
        build "$repo" "$after_dir"
        git -C "$repo" rev-parse "HEAD^{tree}" >"$after_dir/tree"
    fi
    after="$after_dir/recouvrance"
    after_name="$after_name ($(git -C "$repo" rev-parse --short HEAD))"
fi
say "before: $before"
say "after:  $after"

# --- Running one instance -------------------------------------------------------------------

# A scratch config for one instance, `$out/xdg/<name>`: its own folders and MCP port $2 (never
# 47070, the user's own Recouvrance's), write mode off.
scratch() {
    local dir="$out/xdg/$1"
    mkdir -p "$dir/config/recouvrance" "$dir/data" "$dir/cache"
    [ -f "$dir/config/recouvrance/agents.json" ] ||
        printf '{"port": %d, "version": 1}\n' "$2" >"$dir/config/recouvrance/agents.json"
    echo "$dir"
}

# run <label> <binary> <xdg> <snapshot seconds | smoke> [VAR=value...]: one run, its output in
# logs/<label>.log, with its wall time at the end.
run() {
    local label=$1 bin=$2 xdg=$3 mode=$4
    shift 4
    local log="$out/logs/$label.log" start end
    say "run $label"
    start=$(date +%s.%N)
    if [ "$mode" = smoke ]; then
        env XDG_CONFIG_HOME="$xdg/config" XDG_DATA_HOME="$xdg/data" XDG_STATE_HOME="$xdg/data" \
            XDG_CACHE_HOME="$xdg/cache" KUBECONFIG="$kubeconfig" "$@" \
            timeout 900 "$bin" >"$log" 2>&1 || echo "EXIT $?" >>"$log"
    else
        mkdir -p "$out/shots"
        env XDG_CONFIG_HOME="$xdg/config" XDG_DATA_HOME="$xdg/data" XDG_STATE_HOME="$xdg/data" \
            XDG_CACHE_HOME="$xdg/cache" KUBECONFIG="$kubeconfig" "$@" \
            timeout $((minutes * 60 + 900)) "$bin" --snapshot "$out/shots/$label.png" "$mode" >"$log" 2>&1 ||
            echo "EXIT $?" >>"$log"
    fi
    end=$(date +%s.%N)
    echo "WALL $(echo "$end - $start" | bc)" >>"$log"
}

xdg_before=$(scratch before 47199)
xdg_after=$(scratch after 47198)
bin_of() { if [ "$1" = before ]; then echo "$before"; else echo "$after"; fi; }
xdg_of() { if [ "$1" = before ]; then echo "$xdg_before"; else echo "$xdg_after"; fi; }

# A context that refuses connections, next to the real ones.
unreachable="$out/unreachable.yaml"
cat >"$unreachable" <<'EOF'
apiVersion: v1
kind: Config
clusters:
  - name: bench-unreachable
    cluster: {server: "https://127.0.0.1:1"}
users:
  - name: bench-unreachable
    user: {token: unused}
contexts:
  - name: bench-unreachable
    context: {cluster: bench-unreachable, user: bench-unreachable}
EOF

# --- Timings, alternated --------------------------------------------------------------------

for i in $(seq 1 "$rounds"); do
    order="before after"
    [ $((i % 2)) -eq 0 ] && order="after before"
    for b in $order; do
        bin=$(bin_of $b) xdg=$(xdg_of $b)
        ! want smoke || run "smoke$i-$b" "$bin" "$xdg" smoke RECOUVRANCE_SMOKE="$context"
        ! want problems || run "problems$i-$b" "$bin" "$xdg" smoke RECOUVRANCE_SMOKE="$context" RECOUVRANCE_PROBLEMS=twice
        ! want dash || run "dash$i-$b" "$bin" "$xdg" smoke RECOUVRANCE_SMOKE="$context" RECOUVRANCE_DASH=1
        ! want back || run "back$i-$b" "$bin" "$xdg" 8 RECOUVRANCE_CONTEXT="$context" RECOUVRANCE_BACKTIME=pods,deployments,2
        ! want first || run "first$i-$b" "$bin" "$xdg" 30 RECOUVRANCE_CONTEXT="$context" RECOUVRANCE_FIRSTROWS=1
        if want unreachable; then
            kubeconfig="${KUBECONFIG:-$HOME/.kube/config}:$unreachable" \
                run "unreachable$i-$b" "$bin" "$xdg" 40 RECOUVRANCE_CONTEXT="$context,bench-unreachable" RECOUVRANCE_FIRSTROWS=1
        fi
        ! want memory || run "memory$i-$b" "$bin" "$xdg" 45 RECOUVRANCE_CONTEXT="$context" RECOUVRANCE_METER=1
    done
done

# --- Two find_problems calls 60 s apart -----------------------------------------------------

# mcp <port> <token-file> <log>: initialize, then find_problems, 60 s, find_problems again.
mcp() {
    python3 -I - "$1" "$2" "$context" >"$3" 2>&1 <<'PY' || true
import json, sys, time, urllib.request

port, token, context = sys.argv[1], open(sys.argv[2]).read().strip(), sys.argv[3]
url = f"http://127.0.0.1:{port}/mcp"
session = None

def post(msg):
    global session
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
               "Authorization": "Bearer " + token}
    if session:
        headers["Mcp-Session-Id"] = session
    req = urllib.request.Request(url, json.dumps(msg).encode(), headers)
    with urllib.request.urlopen(req, timeout=600) as r:
        session = r.headers.get("Mcp-Session-Id") or session
        body = r.read().decode()
    data = [json.loads(l[5:]) for l in body.splitlines() if l.startswith("data:") and l[5:].strip()]
    return data or ([json.loads(body)] if body.strip().startswith("{") else [])

for _ in range(60):  # the app's MCP server starts with it
    try:
        post({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "release-bench", "version": "1"}}})
        break
    except OSError:
        time.sleep(1)
post({"jsonrpc": "2.0", "method": "notifications/initialized"})
for n in (1, 2):
    if n == 2:
        time.sleep(60)
    t = time.time()
    r = post({"jsonrpc": "2.0", "id": n, "method": "tools/call",
              "params": {"name": "find_problems", "arguments": {"context": context}}})
    took = time.time() - t
    res = (r[-1].get("result") or r[-1].get("error")) if r else {}
    text = " | ".join(c.get("text", "") for c in res.get("content", [])) if isinstance(res, dict) else str(res)
    rows = max(0, len(text.splitlines()) - 1)
    print(f"find_problems {n}: {took:.2f}s error={res.get('isError') if isinstance(res, dict) else None} lines={rows}")
    print("  " + text.splitlines()[0][:200] if text else "  (empty)")
PY
}

if want find_problems; then
    for b in before after; do
        port=47197
        [ $b = after ] && port=47196
        bin=$(bin_of $b) xdg=$(scratch "fp-$b" $port)
        token="$xdg/token"
        [ -s "$token" ] || XDG_CONFIG_HOME="$xdg/config" XDG_DATA_HOME="$xdg/data" "$bin" agent-create bench "$context" >"$token"
        # Reads only: get and list, on the context measured.
        cat >"$xdg/config/recouvrance/rules.yaml" <<EOF
version: 2
agents:
  bench:
    allow:
      - "get,list * --context $context"
EOF
        chmod 600 "$xdg/config/recouvrance/rules.yaml"
        (
            sleep 60
            mcp "$port" "$token" "$out/logs/find_problems-$b.mcp"
        ) &
        run "find_problems-$b" "$bin" "$xdg" 200 RECOUVRANCE_CONTEXT="$context" RECOUVRANCE_METER=1
        wait
    done
fi

# --- Requests per minute, every instance at once --------------------------------------------

if want rate; then
    secs=$((minutes * 60))
    say "Problems page on screen for $minutes minutes, every build at once"
    port=47195
    for b in before after; do
        bin=$(bin_of $b)
        run "rate1-$b" "$bin" "$(scratch "rate1-$b" $port)" 5 RECOUVRANCE_CONTEXT="$context" \
            RECOUVRANCE_PAGE=problems RECOUVRANCE_INPUT="wait:$secs" RECOUVRANCE_METER=1 &
        if grep -qa RECOUVRANCE_WINDOWS "$bin"; then
            run "rate2-$b" "$bin" "$(scratch "rate2-$b" $((port - 1)))" 5 RECOUVRANCE_CONTEXT="$context" \
                RECOUVRANCE_WINDOWS=2 RECOUVRANCE_PAGE=problems RECOUVRANCE_INPUT="wait:$secs" RECOUVRANCE_METER=1 &
        fi
        port=$((port - 2))
    done
    wait
fi

# --- The table ------------------------------------------------------------------------------

python3 -I - "$out/logs" "$tag" "$after_name" "$rounds" >"$out/table.md" <<'PY'
import os, re, sys

logs, tag, after, rounds = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])

def read(name):
    p = os.path.join(logs, name)
    return open(p, errors="replace").read() if os.path.exists(p) else None

def runs(part, b):
    return [t for t in (read(f"{part}{i}-{b}.log") for i in range(1, rounds + 1)) if t is not None]

def groups(pat, t):
    m = re.search(pat, t)
    return m.groups() if m else None

def first(pat, t):
    g = groups(pat, t)
    return g[0] if g else None

def mib(kb):
    return None if kb is None else str(round(int(kb) / 1024))

def secs(s, digits=1):
    return None if s is None else f"{float(s):.{digits}f} s"

def pair(pat, fmt="{} / {}"):
    return lambda t: (lambda g: fmt.format(*g) if g else None)(groups(pat, t))

def metered(t):
    """What the run's RECOUVRANCE_METER lines add up to: requests and MB, every cluster."""
    lines = re.findall(r"^api .+?: (\d+) requests, \d+ failed, ([\d.]+) MB", t, re.M)
    return sum(int(r) for r, _ in lines), sum(float(m) for _, m in lines)

rows = []

def row(name, part, f, note=""):
    cells = []
    for b in ("before", "after"):
        values = [f(t) for t in runs(part, b)]
        cells.append("; ".join(v or "n/a" for v in values) if values else "n/a")
    if cells != ["n/a", "n/a"]:
        rows.append((name, *cells, note))

READS = r"Lists and reads: (\d+) requests \(\d+ failed\), ([\d.]+) MB"
row("Pods list, all namespaces (`RECOUVRANCE_SMOKE`): listed", "smoke", lambda t: first(r"listed at=([\d.]+s)", t))
row("↳ requests / MB", "smoke", pair(READS))
row("↳ peak memory (MiB)", "smoke", lambda t: mib(first(r"VmHWM:\s+(\d+)", t)))
row("Problems scan (`RECOUVRANCE_PROBLEMS=twice`): first", "problems",
    lambda t: secs(first(r"first scan: .*? in ([\d.]+)s", t)))
row("↳ second scan", "problems", lambda t: secs(first(r"second scan: .*? in ([\d.]+)s", t)))
row("↳ requests / MB, first then second", "problems",
    lambda t: (lambda f, s: f"{f[0]} / {f[1]}, then {s[0]} / {s[1]}" if f and s else None)(
        groups(r"first scan: .*?, (\d+) requests, (\d+) MB", t), groups(r"second scan: .*?, (\d+) requests, (\d+) MB", t)))
row("↳ problems found", "problems", lambda t: first(r"first scan: (\d+) problems", t))
row("Workloads overview (`RECOUVRANCE_DASH=1`)", "dash", lambda t: secs(first(r"dashboard load 1: ([\d.]+)s", t), 2))
row("↳ requests / MB", "dash", pair(READS))
row("↳ peak memory (MB)", "dash", lambda t: first(r"peak (\d+) MB", t))
row("Back to pods after deployments (`RECOUVRANCE_BACKTIME`): rows / live", "back",
    lambda t: " · ".join(f"{r} / {l}" for r, l in re.findall(r"back to pods: rows at (\d+ ms), live at (\d+ ms)", t)) or None)
row("↳ requests, MB on each return", "back",
    lambda t: " · ".join(f"{r}, {m} MB" for r, m in re.findall(r"back to pods: .*?, (\d+) requests, ([\d.]+) MB", t)) or None)
row("↳ first visit to deployments: rows / live", "back", pair(r"round 1: deployments: rows at (\d+ ms), live at (\d+ ms)"))
row("First rows after a connect, one context (`RECOUVRANCE_FIRSTROWS=1`): rows / listed", "first",
    pair(r"first-rows: \S+ rows at ([\d.]+s)[\s\S]*?first-rows: \S+ listed at ([\d.]+s)"))
row("↳ plus a context that refuses connections", "unreachable",
    pair(r"first-rows: (?!bench-unreachable)\S+ rows at ([\d.]+s)[\s\S]*?first-rows: (?!bench-unreachable)\S+ listed at ([\d.]+s)"),
    "the reachable one's rows don't wait")
row("Pods list on screen 45 s, badge scanning (`RECOUVRANCE_METER=1`): peak memory (MiB)", "memory",
    lambda t: mib(first(r"peak memory \(VmHWM\): (\d+) kB", t)))
row("↳ requests / MB", "memory", lambda t: (lambda r, m: f"{r:,} / {m:,.0f}" if r else None)(*metered(t)))

def answered(b):
    m = read(f"find_problems-{b}.mcp")
    if m is None:
        return None
    calls = re.findall(r"find_problems \d: ([\d.]+)s error=(\S+)", m)
    return ", then ".join(f"{s} s" + ("" if e in ("False", "None") else " (error)") for s, e in calls) or "no answer"

cells = [answered(b) for b in ("before", "after")]
if any(cells):
    rows.append(("Two `find_problems` 60 s apart, app on the pods list", *(c or "n/a" for c in cells),
                 "the second answers from the first's scan"))

def rate(t):
    if t is None:
        return None
    reqs, mb = metered(t)
    wall = first(r"WALL ([\d.]+)", t)
    if not reqs or not wall:
        return None
    minutes = float(wall) / 60
    peak = mib(first(r"peak memory \(VmHWM\): (\d+) kB", t))
    return f"{reqs / minutes:,.0f}/min ({reqs:,} in {minutes:.1f} min), {mb / minutes:,.0f} MB/min, peak {peak} MiB"

for n, label in ((1, "one window"), (2, "two windows on the same context")):
    cells = [rate(read(f"rate{n}-{b}.log")) for b in ("before", "after")]
    if any(cells):
        rows.append((f"Problems page on screen, {label}", *(c or "n/a" for c in cells), "run at the same time"))

print(f"| Measure | {tag} | {after} | |")
print("|---|---|---|---|")
for name, b, a, note in rows:
    print(f"| {name} | {b} | {a} | {note} |")
failed = [f for f in sorted(os.listdir(logs)) if f.endswith(".log") and re.search(r"^EXIT \d+", read(f), re.M)]
if failed:
    print()
    print("Runs that ended with an error (see their logs): " + ", ".join(failed))
PY

cat "$out/table.md"
say "logs and snapshots in $out; the table in $out/table.md"
