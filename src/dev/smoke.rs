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
                        last.1 = k.ar.group.clone();
                    }
                    print!("{} ", k.ar.kind);
                } else {
                    print!("{}, ", k.label);
                }
            }
            println!();
            return;
        }
        if std::env::var("RECOUVRANCE_PROBLEMS").is_ok() {
            let kinds = k8s::discover(client.clone()).await.unwrap();
            let t = std::time::Instant::now();
            let p = problems::scan(client.clone(), &kinds, None).await.unwrap();
            println!("{} problems in {:?}", p.len(), t.elapsed());
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
        if std::env::var("RECOUVRANCE_DASH").is_ok() {
            let t = std::time::Instant::now();
            let r = crate::ui::dashboard::debug_load(client.clone()).await;
            println!("dashboard load: {:?} ok={}", t.elapsed(), r);
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
            let back: serde_json::Value = serde_yaml_ng::from_str(&yaml).unwrap();
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match futures::FutureExt::now_or_never(rx.next()) {
                Some(Some(k8s::TableMsg::Columns(c))) => {
                    println!("columns: {:?}", c.iter().map(|c| (&c.name, c.priority)).collect::<Vec<_>>())
                }
                Some(Some(k8s::TableMsg::Rows(r))) => {
                    println!("rows={} first={:?}", r.len(), r.first().map(|r| (&r.namespace, &r.name, &r.cells)))
                }
                Some(Some(k8s::TableMsg::Error(e))) => println!("ERROR {e}"),
                Some(Some(k8s::TableMsg::Forbidden(e))) => println!("FORBIDDEN {e}"),
                Some(Some(k8s::TableMsg::Listed)) => println!("listed"),
                Some(None) => break,
                None => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
    });
}
