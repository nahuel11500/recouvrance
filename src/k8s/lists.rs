//! Lean lists: only the fields a page reads, a page at a time, and errors said plainly.

#[allow(unused_imports)]
use super::*;

/// Items per page for `list_lean`: the server never builds one giant response.
const LEAN_PAGE: u32 = 500;

#[derive(Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct LeanList<T> {
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    metadata: ListMeta,
}

/// Every object of a kind, parsed into `T`: a struct with only the fields wanted
/// (everything else is skipped while parsing, never held as JSON), listed in pages.
pub async fn list_lean<T>(client: Client, ar: ApiResource, namespace: Option<String>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    run(async move {
        let url = DynamicObject::url_path(&ar, namespace.as_deref());
        let mut lp = ListParams::default().limit(LEAN_PAGE);
        let mut out = Vec::new();
        loop {
            let page: LeanList<T> = client.request(Request::new(&url).list(&lp)?).await?;
            out.extend(page.items);
            match page.metadata.continue_.filter(|c| !c.is_empty()) {
                Some(token) => lp.continue_token = Some(token),
                None => break,
            }
        }
        Ok(out)
    })
    .await
}

/// Like `list_lean`, in one response from the API server's cache (`resourceVersion=0`):
/// twice as fast as paging a big list, which the server reads again for each page. Parsed
/// as it arrives on a blocking thread, so only the wanted fields are ever held. Pages
/// instead when the server will not answer that way.
pub async fn list_lean_cached<T>(client: Client, ar: ApiResource, namespace: Option<String>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let (c, ar2, ns) = (client.clone(), ar.clone(), namespace.clone());
    let whole = run(async move {
        use tokio_util::compat::FuturesAsyncReadCompatExt as _;
        let url = DynamicObject::url_path(&ar2, ns.as_deref());
        let path = format!("{url}?resourceVersion=0&resourceVersionMatch=NotOlderThan");
        let body = c.request_stream(http::Request::get(&path).body(Vec::new())?).await?;
        let reader = tokio_util::io::SyncIoBridge::new(body.compat());
        let list: LeanList<T> = tokio::task::spawn_blocking(move || {
            serde_json::from_reader(std::io::BufReader::with_capacity(256 * 1024, reader))
        })
        .await??;
        anyhow::ensure!(list.metadata.continue_.as_ref().is_none_or(|c| c.is_empty()), "the API server paged it");
        Ok(list.items)
    })
    .await;
    match whole {
        Ok(items) => Ok(items),
        Err(_) => list_lean(client, ar, namespace).await,
    }
}

/// Why something could not be loaded, as a section header says it: "forbidden"
/// (the user may not read it) or "failed: <the server's message>".
pub fn load_error(e: &anyhow::Error) -> String {
    match api_status(e) {
        Some(403) => "forbidden".into(),
        _ => format!("failed: {}", short_error(e)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lean_lists_skip_other_fields() {
        #[derive(serde::Deserialize)]
        struct Name {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
        }
        let json = r#"{"kind":"PodList","metadata":{"continue":"abc","resourceVersion":"9"},
            "items":[{"metadata":{"name":"a"},"spec":{"containers":[{"name":"c"}]},"status":{"phase":"Running"}}]}"#;
        let list: super::LeanList<Name> = serde_json::from_str(json).unwrap();
        assert_eq!(list.items[0].metadata.name.as_deref(), Some("a"));
        assert_eq!(list.metadata.continue_.as_deref(), Some("abc"));
    }

    #[test]
    fn load_errors() {
        assert_eq!(super::load_error(&anyhow::anyhow!("connection reset")), "failed: connection reset");
    }
}
