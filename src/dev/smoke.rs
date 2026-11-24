//! Checks against a live cluster without the window (`RECOUVRANCE_SMOKE=<context>`):
//! discovery, table watch, list timings, and server dry runs of write actions.

use crate::{k8s, problems, prom};

pub fn run(ctx: String) {
    use futures::StreamExt as _;
    futures::executor::block_on(async move {
        let c = k8s::load_contexts().unwrap();
        println!("contexts={} current={:?}", c.names.len(), c.current);
        let client = k8s::connect(ctx.clone()).await.unwrap();
        // Dry run only.
        if let Ok(target) = std::env::var("RECOUVRANCE_HELMDRY") {
            let (ns, rest) = target.split_once('/').unwrap();
            let (name, rev) = rest.split_once('@').unwrap();
            let args = ["rollback", name, rev, "-n", ns, "--dry-run=client"].map(String::from).to_vec();
            println!("{:?}", k8s::helm(ctx.clone(), args).await);
            return;
        }
        if std::env::var("RECOUVRANCE_SECTIONS").is_ok() {
            let kinds = k8s::discover(client.clone()).await.unwrap();
            let mut last = ("", String::new());
            for k in &kinds {
                if k.section != last.0 {
                    println!("\n[{}]", k.section);
                    last = (k.section, String::new());
                }
                if k8s::is_custom(k) {
                    if k.ar.group != last.1 {
                        print!("\n  {}: ", k.ar.group);
                        last.1.clone_from(&k.ar.group);
                    }
                    print!("{} ", k.ar.kind);
                } else {
                    print!("{}, ", k.label);
                }
            }
            println!();
            return;
        }
        if let Ok(how) = std::env::var("RECOUVRANCE_PROBLEMS") {
            let kinds = k8s::discover(client.clone()).await.unwrap();
            if how == "kinds" {
                problems_by_kind(&client, &kinds).await;
                return;
            }
            if how == "twice" {
                problems_twice(&client, &kinds).await;
                return;
            }
            let t = std::time::Instant::now();
            let scan = problems::scan_partial(client.clone(), &kinds, None, None).await;
            let p = &scan.problems;
            println!("{} problems in {:?}", p.len(), t.elapsed());
            println!("peak memory {} MB (VmHWM)", mb(memory("VmHWM")));
            if let Some(skipped) = scan.skipped_text() {
                println!("could not read {skipped}");
            }
            for x in p.iter().take(12) {
                println!(
                    "{:?} {} {}/{}: {}",
                    x.severity,
                    x.label,
                    x.namespace.clone().unwrap_or_default(),
                    x.name,
                    x.reason
                );
            }
            return;
        }
        if let Ok(target) = std::env::var("RECOUVRANCE_PROM") {
            let ctx = std::env::var("RECOUVRANCE_SMOKE").unwrap_or_default();
            let t = prom::target(&ctx, client.clone()).await;
            println!("target: {t:?}");
            let (ns, pod) = target.split_once('/').unwrap();
            if let Ok(Some(t)) = t {
                let r = prom::pod_usage(client.clone(), t, ns, &[pod.to_string()], 60).await;
                println!("{:?}", r.map(|(c, m)| (c.len(), m.len(), c.last().copied(), m.last().copied())));
            }
            return;
        }
        // Server dry run only: no pod is created.
        if let Ok(node) = std::env::var("RECOUVRANCE_NODESHELLDRY") {
            println!("{:?}", k8s::create_node_shell(client.clone(), node, "busybox:1.37".into(), true).await);
            return;
        }
        // Server dry run only: nothing is added to the pod.
        if let Ok(target) = std::env::var("RECOUVRANCE_DEBUGDRY") {
            let (ns, name) = target.split_once('/').unwrap();
            println!(
                "{:?}",
                k8s::add_debug_container(client.clone(), ns.into(), name.into(), "busybox:1.37".into(), None, true)
                    .await
            );
            return;
        }
        if let Ok(target) = std::env::var("RECOUVRANCE_CRONDRY") {
            let (ns, name) = target.split_once('/').unwrap();
            println!("{:?}", k8s::trigger_cronjob_opts(client.clone(), ns.into(), name.into(), true).await);
            return;
        }
        if std::env::var("RECOUVRANCE_EXPERIMENTS").is_ok() {
            let c = client.clone();
            let _ = k8s::run(async move {
                k8s::experiments::run(c).await;
                Ok(())
            })
            .await;
            return;
        }
        if std::env::var("RECOUVRANCE_LISTTIME").is_ok() {
            let kinds = k8s::discover(client.clone()).await.unwrap();
            let pod = k8s::find_kind(&kinds, "pods").unwrap().clone();
            for _ in 0..2 {
                let t = std::time::Instant::now();
                let (_, rows) = k8s::list_table(client.clone(), pod.clone(), None, None, None).await.unwrap();
                println!("pods list: {} rows in {:?}", rows.len(), t.elapsed());
            }
            // The same request without the Table format, for comparison.
            let t = std::time::Instant::now();
            let n = k8s::list_values(client.clone(), k8s::ar_of::<k8s_openapi::api::core::v1::Pod>(), None, None)
                .await
                .unwrap()
                .len();
            println!("plain list: {n} pods in {:?}", t.elapsed());
            return;
        }
        // RECOUVRANCE_PODRES=<secs>: the usage bars' requests and limits of every pod, read that
        // long as the pods list reads them; then what that read, and the memory it took.
        if let Some(secs) = std::env::var("RECOUVRANCE_PODRES").ok().and_then(|s| s.parse::<u64>().ok()) {
            let start = std::time::Instant::now();
            let (_g, mut rx) =
                k8s::watch_pod_resources(client.clone(), None, k8s::Pause::default(), k8s::Pause::default());
            while start.elapsed() < std::time::Duration::from_secs(secs) {
                if !rx.has_changed().unwrap_or(false) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                let read = rx.borrow_and_update().clone();
                let at = start.elapsed().as_secs_f64();
                match read {
                    Some(Ok(read)) => println!("pod resources: {} pods at {at:.1}s", read.totals.pods),
                    Some(Err(e)) => println!("pod resources: {e} at {at:.1}s"),
                    None => {}
                }
            }
            print_reads(&ctx);
            print_memory();
            return;
        }
        // RECOUVRANCE_CLUSTERLOAD=<rounds>: the cluster overview's load that many times in a row
        // (1 when not a number), each round's time and what it read.
        if let Ok(rounds) = std::env::var("RECOUVRANCE_CLUSTERLOAD") {
            let t = std::time::Instant::now();
            let rounds = rounds.parse().unwrap_or(1);
            let running = crate::ui::cluster::debug_load(ctx.clone(), client.clone(), rounds).await;
            println!("cluster overview load: {:?} running={running}", t.elapsed());
            print_reads(&ctx);
            print_memory();
            return;
        }
        if std::env::var("RECOUVRANCE_HELMLOAD").is_ok() {
            let t = std::time::Instant::now();
            let n = crate::helm::list_releases(client.clone(), None).await.map(|r| r.len());
            println!("helm list: {n:?} releases in {:?}", t.elapsed());
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            let fill = crate::helm::fill_releases(client.clone(), None, tx);
            let count = async {
                use futures::StreamExt as _;
                let (mut n, mut first) = (0, None);
                while let Some(page) = rx.next().await {
                    n += page.len();
                    first.get_or_insert(t.elapsed());
                }
                (n, first)
            };
            let (r, (n, first)) = futures::join!(fill, count);
            println!("helm fill: {n} releases ({r:?}), first page at {first:?}, all at {:?}", t.elapsed());
            return;
        }
        // RECOUVRANCE_DASH=<runs>: the Workloads overview loaded that many times in a row, as
        // the page refreshes; each load's parts, requests and bytes, then the peak memory.
        if let Ok(runs) = std::env::var("RECOUVRANCE_DASH") {
            let runs = runs.parse::<u32>().unwrap_or(1).max(1);
            for run in 1..=runs {
                k8s::meter::reset();
                let t = std::time::Instant::now();
                let r = crate::ui::dashboard::debug_load(client.clone()).await;
                println!("dashboard load {run}: {:?} ok={r}", t.elapsed());
                print_reads(&ctx);
            }
            println!("peak {} MB, now {} MB", mb(memory("VmHWM")), mb(memory("VmRSS")));
            return;
        }
        // RECOUVRANCE_EVENTS=<seconds>: the Timeline's "All events" on every namespace for that
        // long, each time it gets them, then what it cost (requests, bytes, peak memory).
        if let Some(secs) = std::env::var("RECOUVRANCE_EVENTS").ok().and_then(|s| s.parse::<u64>().ok()) {
            let start = std::time::Instant::now();
            let end = start + std::time::Duration::from_secs(secs);
            let every = || crate::settings::secs(crate::settings::get().overview_refresh);
            // Listed once, then watched: the rows come again when they changed, at most every
            // refresh interval.
            let (_watch, mut rx) =
                k8s::watch_events(client.clone(), None, true, k8s::EVENTS_KEPT, every, k8s::Pause::default());
            while std::time::Instant::now() < end {
                match futures::FutureExt::now_or_never(rx.next()) {
                    Some(Some(k8s::EventsMsg::Rows(r))) => println!(
                        "events: {} rows, the newest {} of {} events, at {}s",
                        r.rows.len(),
                        r.kept,
                        r.total,
                        start.elapsed().as_secs()
                    ),
                    Some(Some(k8s::EventsMsg::Error(e))) => {
                        println!("events: ERROR {e} at {}s", start.elapsed().as_secs());
                    }
                    Some(None) => break,
                    None => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
            crate::dev::usage::print();
            return;
        }
        // RECOUVRANCE_EDIT=ns/deploy: YAML round trip + server-side dry-run replace.
        if let Ok(target) = std::env::var("RECOUVRANCE_EDIT") {
            let (ns, name) = target.split_once('/').unwrap();
            let kinds = k8s::discover(client.clone()).await.unwrap();
            let kind = k8s::find_kind(&kinds, "deployments").unwrap().clone();
            let obj = k8s::ObjectRef {
                kind: kind.clone(),
                name: name.into(),
                namespace: Some(ns.into()),
                uid: String::new(),
            };
            let value = k8s::get_object(client.clone(), obj).await.unwrap();
            let yaml = k8s::to_yaml(&value, false, false).unwrap();
            let back: serde_json::Value = crate::yaml::from_str(&yaml).unwrap();
            println!("{:?}", k8s::replace(client.clone(), kind, Some(ns.into()), back, true).await);
            return;
        }
        // RECOUVRANCE_PF=ns/pod:port checks port-forward with one HTTP GET /metrics.
        if let Ok(pf) = std::env::var("RECOUVRANCE_PF") {
            let (target, port) = pf.rsplit_once(':').unwrap();
            let (ns, pod) = target.split_once('/').unwrap();
            let (_g, mut rx) = k8s::port_forward(
                client.clone(),
                ns.into(),
                pod.into(),
                None,
                "127.0.0.1".into(),
                0,
                port.parse().unwrap(),
            );
            let Some(k8s::ForwardMsg::Listening(local)) = rx.next().await else { panic!("no listener") };
            println!("listening on {local}");
            let out = std::process::Command::new("curl")
                .args(["-s", "-m", "10", &format!("http://127.0.0.1:{local}/metrics")])
                .output()
                .unwrap();
            let body = String::from_utf8_lossy(&out.stdout);
            println!("got {} bytes; first line: {:?}", body.len(), body.lines().next());
            return;
        }
        let t = std::time::Instant::now();
        let kinds = k8s::discover(client.clone()).await.unwrap();
        println!(
            "kinds={} in {:?}; custom={}",
            kinds.len(),
            t.elapsed(),
            kinds.iter().filter(|k| k.section == k8s::CUSTOM_SECTION).count()
        );
        let pod = kinds.iter().find(|k| k.is("", "Pod")).unwrap().clone();
        let (_g, mut rx) = k8s::watch_table(client.clone(), pod, None, Default::default());
        // RECOUVRANCE_WATCHSECS: watch longer (and timestamp each message), to check recovery.
        let secs = std::env::var("RECOUVRANCE_WATCHSECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            match futures::FutureExt::now_or_never(rx.next()) {
                Some(Some(k8s::TableMsg::Columns(c))) => {
                    println!("columns: {:?}", c.iter().map(|c| (&c.name, c.priority)).collect::<Vec<_>>());
                }
                Some(Some(k8s::TableMsg::Rows(r))) => {
                    let first = r.first().map(|r| (&r.namespace, &r.name, &r.cells));
                    // CI greps for lines starting with "rows=": the time goes at the end.
                    println!("rows={} first={first:?} at={}s", r.len(), start.elapsed().as_secs());
                }
                Some(Some(k8s::TableMsg::Error(e))) => println!("ERROR {e} at={}s", start.elapsed().as_secs()),
                Some(Some(k8s::TableMsg::Forbidden(e))) => println!("FORBIDDEN {e}"),
                Some(Some(k8s::TableMsg::AllRows)) => println!("all rows at={}s", start.elapsed().as_secs()),
                // When the list went live, and what it took to get there: the requests and
                // bytes since connecting, and the peak memory.
                Some(Some(k8s::TableMsg::Listed)) => {
                    println!("listed at={:.1}s", start.elapsed().as_secs_f64());
                    print_reads(&ctx);
                    print_memory();
                }
                Some(None) => break,
                None => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
    });
}

/// Bytes this context's requests brought so far, after decompression (Settings → API requests).
pub fn bytes_read(ctx: &str) -> u64 {
    let seen = k8s::meter::snapshot(k8s::meter::now_minute());
    seen.classes.iter().filter(|(c, ..)| c == ctx).map(|(_, _, stats)| stats.bytes).sum()
}

/// What this context's requests brought since the meter's last reset by kind, as Settings →
/// API requests shows it, and the heaviest of them with their time to the last byte.
fn print_reads(ctx: &str) {
    let seen = k8s::meter::snapshot(k8s::meter::now_minute());
    for (_, class, stats) in seen.classes.iter().filter(|(c, ..)| c == ctx) {
        let megabytes = stats.bytes as f64 / 1e6;
        println!("{}: {} requests ({} failed), {megabytes:.1} MB", class.label(), stats.count, stats.errors);
    }
    for heavy in seen.heaviest.iter().filter(|h| h.context == ctx) {
        let took = heavy.last_byte.map_or_else(|| "-".to_string(), |d| format!("{:.2} s", d.as_secs_f64()));
        println!("heaviest: {} {} {:.1} MB in {took}", heavy.method, heavy.path, heavy.bytes as f64 / 1e6);
    }
}

/// The process's peak and current memory, as Linux counts them.
fn print_memory() {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")) {
        println!("{line}");
    }
}

/// Each check of the Problems scan alone, one after the other (`RECOUVRANCE_PROBLEMS=kinds`):
/// its time, and the process's peak resident memory while it ran, with what it held before.
/// The memory is Linux's: the peak is reset to what is held before each check.
async fn problems_by_kind(client: &kube::Client, kinds: &[k8s::Kind]) {
    let with_services = |k: &k8s::Kind| problems::WITH_SERVICES.iter().any(|(g, n)| k.is(g, n));
    for (group, name) in problems::CHECKED {
        let Some(kind) = kinds.iter().find(|k| k.is(group, name)) else { continue };
        // Checked with the Services they point to.
        if with_services(kind) {
            continue;
        }
        let mut checked = vec![kind.clone()];
        if kind.is("", "Service") {
            checked.extend(kinds.iter().filter(|k| with_services(k)).cloned());
        }
        // What the check before freed goes back to the system after a moment.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let _ = std::fs::write("/proc/self/clear_refs", "5");
        let before = memory("VmRSS");
        let t = std::time::Instant::now();
        let scan = problems::scan_partial(client.clone(), &checked, None, None).await;
        println!(
            "{name}: {} problems in {:?}, peak {} MB from {} MB",
            scan.problems.len(),
            t.elapsed(),
            mb(memory("VmHWM")),
            mb(before)
        );
        if let Some(skipped) = scan.skipped_text() {
            println!("  could not read {skipped}");
        }
    }
}

/// Two Problems scans one after the other (`RECOUVRANCE_PROBLEMS=twice`): each one's time,
/// and the requests it sent and the bytes it received, from the API requests meter. The
/// second shows what a scan costs when the one before can be reused.
async fn problems_twice(client: &kube::Client, kinds: &[k8s::Kind]) {
    for run in ["first", "second"] {
        k8s::meter::reset();
        let t = std::time::Instant::now();
        let scan = problems::scan_partial(client.clone(), kinds, None, None).await;
        let took = t.elapsed();
        let snapshot = k8s::meter::snapshot(k8s::meter::now_minute());
        let requests: u64 = snapshot.classes.iter().map(|(_, _, s)| s.count).sum();
        let bytes: u64 = snapshot.classes.iter().map(|(_, _, s)| s.bytes).sum();
        println!(
            "{run} scan: {} problems in {took:?}, {requests} requests, {} MB received",
            scan.problems.len(),
            bytes / 1_000_000
        );
    }
}

/// A memory figure of this process in MB, from /proc/self/status (`VmHWM`: its peak, `VmRSS`:
/// now); None off Linux.
fn memory(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = status.lines().find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))?;
    Some(kb.trim().trim_end_matches("kB").trim().parse::<u64>().ok()? / 1024)
}

fn mb(v: Option<u64>) -> String {
    v.map_or_else(|| "?".into(), |v| v.to_string())
}
