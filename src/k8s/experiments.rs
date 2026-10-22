//! Dev-only measurements of list strategies against a live cluster (read-only):
//! `RECOUVRANCE_SMOKE=<context> RECOUVRANCE_EXPERIMENTS=1`.

#[allow(unused_imports)]
use super::*;

use super::table::{ServerTable, TABLE_ACCEPT};

fn table_get(path: &str) -> Result<http::Request<Vec<u8>>> {
    let mut req = http::Request::get(path).body(Vec::new())?;
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
    Ok(req)
}

/// A paged Table list of pods: total time, time to the first page, pages, rows, bytes.
async fn paged(client: &Client, extra: &str) -> Result<String> {
    let start = std::time::Instant::now();
    let (mut pages, mut rows, mut bytes, mut first) = (0, 0, 0usize, None);
    let mut token: Option<String> = None;
    loop {
        let cont = token.as_ref().map(|t| format!("&continue={t}")).unwrap_or_default();
        let include = if extra.contains("includeObject") { "" } else { "&includeObject=Metadata" };
        let path = format!("/api/v1/pods?limit=2000{include}{extra}{cont}");
        let text = client.request_text(table_get(&path)?).await?;
        first.get_or_insert(start.elapsed());
        bytes += text.len();
        let page: ServerTable = serde_json::from_str(&text)?;
        pages += 1;
        rows += page.rows.len();
        match page.metadata.continue_.filter(|c| !c.is_empty()) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(format!(
        "{rows} rows, {pages} pages, {:.1} MB, first page {:?}, total {:?}",
        bytes as f64 / 1e6,
        first.unwrap_or_default(),
        start.elapsed()
    ))
}

/// A streaming list (watch with sendInitialEvents) in Table form: does it work, and how long
/// until the initial-events-end bookmark.
async fn streaming(client: &Client) -> Result<String> {
    use futures::AsyncBufReadExt as _;
    let start = std::time::Instant::now();
    let path = "/api/v1/pods?watch=true&sendInitialEvents=true&resourceVersionMatch=NotOlderThan&allowWatchBookmarks=true&includeObject=Metadata&timeoutSeconds=120";
    let stream = client.request_stream(table_get(path)?).await?;
    let mut lines = stream.lines();
    let (mut added, mut tables, mut bytes, mut first) = (0usize, 0usize, 0usize, None);
    while let Some(line) = lines.next().await {
        let line = line?;
        bytes += line.len();
        first.get_or_insert(start.elapsed());
        let v: serde_json::Value = serde_json::from_str(&line)?;
        match v["type"].as_str() {
            Some("ADDED") => {
                added += 1;
                if v["object"]["kind"] == "Table" {
                    tables += v["object"]["rows"].as_array().map_or(0, Vec::len);
                }
            }
            Some("BOOKMARK") => {
                let end = v.pointer("/object/metadata/annotations/k8s.io~1initial-events-end").is_some()
                    || v.pointer("/object/rows/0/object/metadata/annotations/k8s.io~1initial-events-end").is_some();
                if end {
                    return Ok(format!(
                        "works: {added} ADDED events ({tables} Table rows), {:.1} MB, first event {:?}, initial-events-end after {:?}",
                        bytes as f64 / 1e6,
                        first.unwrap_or_default(),
                        start.elapsed()
                    ));
                }
            }
            Some("ERROR") => return Ok(format!("refused: {}", v["object"]["message"])),
            _ => {}
        }
    }
    Ok(format!("stream ended without initial-events-end after {added} events, {:?}", start.elapsed()))
}

/// Watch pods (Table form, compressed by default) for `secs` and count frames that fail to
/// parse: sofka says compressed watches can come out garbled.
async fn watch_decode(client: &Client, secs: u64) -> Result<String> {
    use futures::AsyncBufReadExt as _;
    let list: ServerTable = client.request(table_get("/api/v1/pods?limit=1&includeObject=Metadata")?).await?;
    let rv = list.metadata.resource_version.unwrap_or_default();
    let path = format!(
        "/api/v1/pods?watch=true&resourceVersion={rv}&allowWatchBookmarks=true&includeObject=Metadata&timeoutSeconds={secs}"
    );
    let stream = client.request_stream(table_get(&path)?).await?;
    let mut lines = stream.lines();
    let (mut ok, mut bad, mut bookmarks) = (0usize, 0usize, 0usize);
    while let Some(line) = lines.next().await {
        match line.map_err(anyhow::Error::from).and_then(|l| Ok(serde_json::from_str::<serde_json::Value>(&l)?)) {
            Ok(v) if v["type"] == "BOOKMARK" => bookmarks += 1,
            Ok(_) => ok += 1,
            Err(_) => bad += 1,
        }
    }
    Ok(format!("{ok} events, {bookmarks} bookmarks, {bad} frames that failed to decode, over {secs}s"))
}

/// Where a whole list's time goes: download (with gzip), JSON parse, rows.
async fn breakdown(client: &Client, encoding: &'static str) -> Result<String> {
    let start = std::time::Instant::now();
    let path = "/api/v1/pods?includeObject=Metadata&resourceVersion=0&resourceVersionMatch=NotOlderThan";
    let mut req = table_get(path)?;
    req.headers_mut().insert(http::header::ACCEPT_ENCODING, http::HeaderValue::from_static(encoding));
    let text = client.request_text(req).await?;
    let got = start.elapsed();
    let t = std::time::Instant::now();
    let page: ServerTable = serde_json::from_str(&text)?;
    let parsed = t.elapsed();
    let t = std::time::Instant::now();
    let extras = Extras::default();
    let rows: Vec<_> = page.rows.into_iter().filter_map(|r| to_row(r, &extras)).collect();
    let converted = t.elapsed();
    // How much of each object is managedFields (sent but never used).
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let mut managed = 0usize;
    for r in v["rows"].as_array().into_iter().flatten() {
        managed += r.pointer("/object/metadata/managedFields").map_or(0, |m| m.to_string().len());
    }
    Ok(format!(
        "{} rows, {:.1} MB ({:.1} MB of it managedFields): download+decompress {got:?}, parse {parsed:?}, rows {converted:?}",
        rows.len(),
        text.len() as f64 / 1e6,
        managed as f64 / 1e6
    ))
}

/// Pods listed one namespace at a time, `parallel` namespaces at once (Table, metadata,
/// gzip): several connections, where one is held back by latency.
async fn by_namespace(client: &Client, parallel: usize, include: &'static str) -> Result<String> {
    let start = std::time::Instant::now();
    let ns: serde_json::Value = client.request(table_get("/api/v1/namespaces?includeObject=None")?).await?;
    let names: Vec<String> =
        ns["rows"].as_array().into_iter().flatten().filter_map(|r| r["cells"][0].as_str().map(String::from)).collect();
    let listed = start.elapsed();
    let count = names.len();
    let jobs: Vec<_> = names
        .into_iter()
        .map(|n| {
            let client = client.clone();
            let path = format!(
                "/api/v1/namespaces/{n}/pods?includeObject={include}&resourceVersion=0&resourceVersionMatch=NotOlderThan"
            );
            async move {
                let text = client.request_text(table_get(&path)?).await?;
                let page: ServerTable = serde_json::from_str(&text)?;
                Ok::<_, anyhow::Error>((page.rows.len(), text.len()))
            }
        })
        .collect();
    let results: Vec<Result<(usize, usize)>> = futures::stream::iter(jobs).buffer_unordered(parallel).collect().await;
    let (mut rows, mut bytes, mut errors) = (0, 0, 0);
    for r in results {
        match r {
            Ok((n, b)) => {
                rows += n;
                bytes += b;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(format!(
        "{rows} rows from {} namespaces ({errors} failed), {:.1} MB, namespaces in {listed:?}, total {:?}",
        count,
        bytes as f64 / 1e6,
        start.elapsed()
    ))
}

pub async fn run(client: Client) {
    if std::env::var("RECOUVRANCE_NOOBJECT").is_ok() {
        // Interleaved, since the link's speed changes from one minute to the next.
        for _ in 0..3 {
            let base = "&resourceVersion=0&resourceVersionMatch=NotOlderThan";
            let meta = paged(&client, base).await.unwrap_or_else(|e| format!("error {e:#}"));
            let none =
                paged(&client, &format!("{base}&includeObject=None")).await.unwrap_or_else(|e| format!("error {e:#}"));
            println!("  with metadata:    {meta}\n  cells only (None): {none}");
        }
        return;
    }
    if std::env::var("RECOUVRANCE_PARALLEL").is_ok() {
        let include = if std::env::var("RECOUVRANCE_NOOBJECT_NS").is_ok() { "None" } else { "Metadata" };
        for p in [16, 32, 64, 32] {
            println!(
                "by namespace x{p:<2} ({include}): {}",
                by_namespace(&client, p, include).await.unwrap_or_else(|e| format!("error {e:#}"))
            );
        }
        println!(
            "single rv=0 list: {}",
            paged(&client, "&resourceVersion=0&resourceVersionMatch=NotOlderThan")
                .await
                .unwrap_or_else(|e| format!("error {e:#}"))
        );
        return;
    }
    if std::env::var("RECOUVRANCE_BREAKDOWN").is_ok() {
        for enc in ["identity", "gzip", "identity", "gzip"] {
            println!("{enc:>8}: {}", breakdown(&client, enc).await.unwrap_or_else(|e| format!("error {e:#}")));
        }
        return;
    }
    let version: serde_json::Value =
        client.request(http::Request::get("/version").body(Vec::new()).unwrap()).await.unwrap_or_default();
    println!("server {}", version["gitVersion"].as_str().unwrap_or("?"));
    for round in 1..=2 {
        println!("round {round}");
        println!(
            "  paged, consistent (today): {}",
            paged(&client, "").await.unwrap_or_else(|e| format!("error {e:#}"))
        );
        println!(
            "  paged, resourceVersion=0:  {}",
            paged(&client, "&resourceVersion=0").await.unwrap_or_else(|e| format!("error {e:#}"))
        );
        println!(
            "  paged, rv=0 NotOlderThan:  {}",
            paged(&client, "&resourceVersion=0&resourceVersionMatch=NotOlderThan")
                .await
                .unwrap_or_else(|e| format!("error {e:#}"))
        );
        println!(
            "  streaming list:            {}",
            streaming(&client).await.unwrap_or_else(|e| format!("error {e:#}"))
        );
    }
    let secs = std::env::var("RECOUVRANCE_WATCHSECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    println!("watch decode: {}", watch_decode(&client, secs).await.unwrap_or_else(|e| format!("error {e:#}")));
}
