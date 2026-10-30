//! Helm releases, read from Helm's own storage (Secrets labelled `owner=helm`): listing
//! them, a release's history, and masking the secrets their values and manifests hold.

use std::io::Read as _;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, ListParams};
use serde_json::Value;

use crate::k8s;

/// Helm's Secrets of current revisions (not the whole history).
const CURRENT: &str = "owner=helm,status!=superseded";
/// Releases read per request while the list fills in.
const FILL_PAGE: u32 = 100;

#[derive(Clone)]
pub struct Release {
    pub context: String,
    pub name: String,
    pub namespace: String,
    pub revision: i64,
    pub status: String,
    pub chart: String,
    pub chart_version: String,
    pub app_version: String,
    pub updated: Option<jiff::Timestamp>,
    pub description: String,
    pub notes: String,
    pub values: Value,
    pub manifest: String,
    /// Notes, values and manifest are read. The list keeps releases without them (a
    /// thousand manifests weigh hundreds of MB); a release's page reads them again.
    pub loaded: bool,
}

/// Helm keeps a release revision in one Secret (at most ~1 MiB); more is not Helm's.
const MAX_RELEASE_DATA: usize = 4 << 20;

/// Decompressed release JSON: generous for huge charts, small enough that a gzip bomb fails.
const MAX_RELEASE_JSON: u64 = 32 << 20;

/// Whether `name` can be a Helm release name (also a valid label value): lowercase
/// letters, digits, '-' and '.', at most 53 characters.
pub(crate) fn valid_release_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 53
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Secret data -> base64 (Helm's own) -> gzip -> JSON.
///
/// The release's identity comes from the Secret itself (its namespace, its `name` label and
/// Helm's own Secret name), never from the payload: anyone who can create a Secret in their
/// namespace could otherwise label it `owner=helm` and claim to be `kube-system/cilium`.
pub(crate) fn decode(secret: &Secret) -> Result<Release> {
    let meta = &secret.metadata;
    let namespace = meta.namespace.clone().context("no namespace")?;
    let label = |k: &str| meta.labels.as_ref().and_then(|l| l.get(k)).cloned();
    let name = label("name").filter(|n| valid_release_name(n)).context("no valid release name label")?;
    let raw = secret.data.as_ref().and_then(|d| d.get("release")).context("no release data")?;
    anyhow::ensure!(raw.0.len() <= MAX_RELEASE_DATA, "release data too large");
    let inner = base64::engine::general_purpose::STANDARD.decode(&raw.0)?;
    let mut json = Vec::new();
    if inner.starts_with(&[0x1f, 0x8b]) {
        // One byte past the cap tells "too large" from "exactly the cap".
        flate2::read::GzDecoder::new(&inner[..]).take(MAX_RELEASE_JSON + 1).read_to_end(&mut json)?;
        anyhow::ensure!(json.len() as u64 <= MAX_RELEASE_JSON, "release data decompresses to more than 32 MiB");
    } else {
        json = inner;
    }
    let r: Value = serde_json::from_slice(&json)?;
    let s = |p: &str| r.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let revision = r["version"].as_i64().unwrap_or(0);
    // A payload naming another release, or a Secret not named the way Helm names it, is not Helm's.
    anyhow::ensure!(s("/name") == name && s("/namespace") == namespace, "release payload names another release");
    anyhow::ensure!(
        meta.name.as_deref() == Some(format!("sh.helm.release.v1.{name}.v{revision}").as_str()),
        "not a Helm release Secret name"
    );
    if let Some(v) = label("version") {
        anyhow::ensure!(v == revision.to_string(), "revision label and payload disagree");
    }
    Ok(Release {
        context: String::new(),
        name,
        namespace,
        revision,
        status: s("/info/status"),
        chart: s("/chart/metadata/name"),
        chart_version: s("/chart/metadata/version"),
        app_version: s("/chart/metadata/appVersion"),
        updated: r.pointer("/info/last_deployed").and_then(Value::as_str).and_then(|t| t.parse().ok()),
        description: s("/info/description"),
        notes: s("/info/notes"),
        values: r.get("config").cloned().unwrap_or(Value::Null),
        manifest: s("/manifest"),
        loaded: true,
    })
}

/// A release as the list shows it: without notes, values and manifest.
pub(crate) fn summary(mut r: Release) -> Release {
    r.notes = String::new();
    r.values = Value::Null;
    r.manifest = String::new();
    r.loaded = false;
    r
}

/// A release from its Secret's labels alone, with the same identity checks as `decode`:
/// what the list shows until the payload is read (chart and app version come from there).
pub(crate) fn stub(meta: &ObjectMeta) -> Option<Release> {
    let namespace = meta.namespace.clone()?;
    let label = |k: &str| meta.labels.as_ref().and_then(|l| l.get(k)).cloned();
    let name = label("name").filter(|n| valid_release_name(n))?;
    let revision: i64 = label("version")?.parse().ok()?;
    if meta.name.as_deref() != Some(format!("sh.helm.release.v1.{name}.v{revision}").as_str()) {
        return None;
    }
    // Helm writes when a revision changed (seconds) next to its status.
    let updated = label("modifiedAt")
        .and_then(|t| t.parse().ok())
        .and_then(|t| jiff::Timestamp::from_second(t).ok())
        .or_else(|| meta.creation_timestamp.as_ref().map(|t| t.0));
    Some(Release {
        context: String::new(),
        name,
        namespace,
        revision,
        status: label("status").unwrap_or_default(),
        chart: String::new(),
        chart_version: String::new(),
        app_version: String::new(),
        updated,
        description: String::new(),
        notes: String::new(),
        values: Value::Null,
        manifest: String::new(),
        loaded: false,
    })
}

async fn secrets(client: Client, namespace: Option<String>, labels: String) -> Result<Vec<Secret>> {
    k8s::run(async move {
        let api: Api<Secret> = match &namespace {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        Ok(api.list(&ListParams::default().labels(&labels)).await?.items)
    })
    .await
}

/// The current revision of every release, from the Secrets' labels: one small list, where
/// reading the payloads of a thousand releases takes half a minute.
pub async fn list_releases(client: Client, namespace: Option<String>) -> Result<Vec<Release>> {
    k8s::run(async move {
        let api: Api<Secret> = match &namespace {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        let items = api.list_metadata(&ListParams::default().labels(CURRENT)).await?.items;
        Ok(latest_revisions(items.iter().filter_map(|m| stub(&m.metadata)).collect()))
    })
    .await
}

/// The same releases with their chart and app version, page by page as they are read
/// (sent to `pages`), decoded off the UI thread.
pub async fn fill_releases(
    client: Client,
    namespace: Option<String>,
    pages: futures::channel::mpsc::UnboundedSender<Vec<Release>>,
) -> Result<()> {
    k8s::run(async move {
        let api: Api<Secret> = match &namespace {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        let mut lp = ListParams::default().labels(CURRENT).limit(FILL_PAGE);
        loop {
            let list = api.list(&lp).await?;
            let page = list.items.iter().filter_map(|s| decode(s).ok()).map(summary).collect();
            if pages.unbounded_send(page).is_err() {
                return Ok(()); // the list was closed
            }
            match list.metadata.continue_.filter(|c| !c.is_empty()) {
                Some(token) => lp.continue_token = Some(token),
                None => return Ok(()),
            }
        }
    })
    .await
}

/// What `read` makes of each current release's manifest, next to the release (kept without
/// it), page by page: the manifests of a thousand releases are never held at once. One answer
/// per release, its newest revision; with the number of releases that could not be read.
pub(crate) async fn scan_manifests<T: Send + 'static>(
    client: Client,
    read: impl Fn(&Release) -> T + Send + 'static,
) -> Result<(Vec<(Release, T)>, usize)> {
    k8s::run(async move {
        let api: Api<Secret> = Api::all(client);
        let mut lp = ListParams::default().labels(CURRENT).limit(FILL_PAGE);
        let (mut out, mut unreadable) = (Vec::new(), 0);
        loop {
            let list = api.list(&lp).await?;
            for secret in &list.items {
                match decode(secret) {
                    Ok(r) => {
                        let found = read(&r);
                        out.push((summary(r), found));
                    }
                    Err(_) => unreadable += 1,
                }
            }
            match list.metadata.continue_.filter(|c| !c.is_empty()) {
                Some(token) => lp.continue_token = Some(token),
                None => break,
            }
        }
        out.sort_by(|(a, _), (b, _)| {
            (&a.namespace, &a.name, std::cmp::Reverse(a.revision)).cmp(&(
                &b.namespace,
                &b.name,
                std::cmp::Reverse(b.revision),
            ))
        });
        out.dedup_by(|(a, _), (b, _)| a.namespace == b.namespace && a.name == b.name);
        Ok((out, unreadable))
    })
    .await
}

/// One release per (namespace, name): the newest revision, when an upgrade is in flight.
pub(crate) fn latest_revisions(mut releases: Vec<Release>) -> Vec<Release> {
    releases.sort_by(|a, b| {
        (&a.namespace, &a.name, std::cmp::Reverse(a.revision)).cmp(&(
            &b.namespace,
            &b.name,
            std::cmp::Reverse(b.revision),
        ))
    });
    releases.dedup_by(|a, b| a.namespace == b.namespace && a.name == b.name);
    releases
}

pub async fn history(client: Client, namespace: String, name: String) -> Result<Vec<Release>> {
    // The name went through `decode`, but this builds a selector: check it again.
    anyhow::ensure!(valid_release_name(&name), "invalid release name {name:?}");
    let items = secrets(client, Some(namespace.clone()), format!("owner=helm,name={name}")).await?;
    // Unzipping and parsing every revision is slow for big charts: not on the UI thread.
    k8s::run(async move {
        let mut revs: Vec<Release> = items
            .iter()
            .filter_map(|s| decode(s).ok())
            .filter(|r| r.name == name && r.namespace == namespace)
            .collect();
        revs.sort_by_key(|r| std::cmp::Reverse(r.revision));
        Ok(revs)
    })
    .await
}

/// The newest revision of a release, from its Secrets' labels (no payload read).
pub(crate) async fn newest_revision(client: Client, namespace: String, name: String) -> Result<Option<i64>> {
    anyhow::ensure!(valid_release_name(&name), "invalid release name {name:?}");
    k8s::run(async move {
        let api: Api<Secret> = Api::namespaced(client, &namespace);
        let lp = ListParams::default().labels(&format!("owner=helm,name={name}"));
        let items = api.list_metadata(&lp).await?.items;
        Ok(items
            .iter()
            .filter_map(|m| stub(&m.metadata))
            .filter(|r| r.name == name && r.namespace == namespace)
            .map(|r| r.revision)
            .max())
    })
    .await
}

/// Why revision `revision` of a release can't be read, when its Secret is there but its payload
/// doesn't decode (too large, say): the history leaves it out, so no page can show it.
pub(crate) async fn unreadable_revision(
    client: Client,
    namespace: String,
    name: String,
    revision: i64,
) -> Option<String> {
    k8s::run(async move {
        let api: Api<Secret> = Api::namespaced(client, &namespace);
        let secret = api.get(&format!("sh.helm.release.v1.{name}.v{revision}")).await?;
        Ok(decode(&secret).err().map(|e| format!("{e:#}")))
    })
    .await
    .ok()
    .flatten()
}

/// Values as YAML; secret-looking keys are masked unless revealed.
pub(crate) fn values_yaml(values: &Value, reveal: bool) -> String {
    if values.is_null() {
        return "# no user-supplied values\n".into();
    }
    let mut v = values.clone();
    if !reveal {
        mask_secrets(&mut v);
    }
    crate::yaml::to_string(&v).unwrap_or_default()
}

/// Mask the values under secret-looking keys: a string or number, or each one in a list of
/// them (`passwords: [p1, p2]`). An environment variable (`{name: DB_PASSWORD, value: ...}`)
/// is masked by its name.
pub(crate) fn mask_secrets(v: &mut Value) {
    const HINTS: [&str; 9] =
        ["password", "passwd", "secret", "token", "apikey", "api_key", "credential", "privatekey", "private_key"];
    let secret_name = |k: &str| {
        let key = k.to_lowercase().replace('-', "_");
        HINTS.iter().any(|h| key.contains(h)) || key.ends_with("key") && !key.ends_with("publickey")
    };
    let scalar = |v: &Value| v.is_string() || v.is_number();
    let mask = || Value::String("••••••".into());
    match v {
        Value::Object(map) => {
            let secret_env = map.get("name").and_then(Value::as_str).is_some_and(secret_name);
            for (k, val) in map.iter_mut() {
                let secretish = secret_name(k.as_str()) || secret_env && k == "value";
                match val {
                    val if secretish && scalar(val) => *val = mask(),
                    Value::Array(items) if secretish => {
                        for item in items {
                            if scalar(item) { *item = mask() } else { mask_secrets(item) }
                        }
                    }
                    val => mask_secrets(val),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_secrets),
        _ => {}
    }
}

/// The rendered manifest with the values of its Secrets masked (keys stay visible); other
/// documents, and the `# Source:` comments, are kept as they are.
pub(crate) fn mask_manifest(manifest: &str) -> String {
    let mut out = String::with_capacity(manifest.len());
    let mut doc = String::new();
    let flush = |doc: &mut String, out: &mut String| {
        out.push_str(&mask_manifest_doc(doc));
        doc.clear();
    };
    for line in manifest.split_inclusive('\n') {
        // `--- # Source: chart/templates/secret.yaml` starts a document too.
        if crate::yaml::doc_start(line.trim_end()).is_some() {
            flush(&mut doc, &mut out);
            out.push_str(line);
        } else {
            doc.push_str(line);
        }
    }
    flush(&mut doc, &mut out);
    out
}

fn mask_manifest_doc(doc: &str) -> String {
    if !doc.contains("Secret") {
        return doc.to_string();
    }
    let comments: String =
        doc.lines().take_while(|l| l.trim_start().starts_with('#')).map(|l| format!("{l}\n")).collect();
    match crate::yaml::from_str::<Value>(doc) {
        Ok(mut v) if v["kind"] == "Secret" => {
            k8s::mask_secret_values(&mut v);
            comments + &crate::yaml::to_string(&v).unwrap_or_default()
        }
        Ok(_) => doc.to_string(),
        // Unreadable, but it may be a Secret: hide it rather than show its values.
        Err(_) if doc.lines().any(|l| l.trim_start().starts_with("kind:") && l.contains("Secret")) => {
            comments + "# A Secret (its values are hidden; reveal secrets to see it)\n"
        }
        Err(_) => doc.to_string(),
    }
}

/// The releases of every cluster that answered, and "context: error" for the others; an
/// error alone when no cluster answered.
pub(crate) fn merge_clusters(
    results: Vec<(String, Result<Vec<Release>>)>,
) -> (Result<Vec<Release>, String>, Vec<String>) {
    let single = results.len() == 1;
    let (mut releases, mut errors, mut answered) = (Vec::new(), Vec::new(), false);
    for (ctx, r) in results {
        match r {
            Ok(rels) => {
                answered = true;
                releases.extend(rels);
            }
            Err(e) if single => errors.push(format!("{e:#}")),
            Err(e) => errors.push(format!("{ctx}: {e:#}")),
        }
    }
    if answered || errors.is_empty() { (Ok(releases), errors) } else { (Err(errors.join("\n")), Vec::new()) }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write as _;

    use super::*;

    /// A Helm release Secret: `meta` is (Secret name, namespace, `name` label), `payload` the JSON.
    pub(crate) fn helm_secret(meta: (&str, &str, &str), payload: &Value, gzip: bool) -> Secret {
        let mut json = serde_json::to_vec(payload).unwrap();
        if gzip {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&json).unwrap();
            json = gz.finish().unwrap();
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(json);
        let mut s = Secret::default();
        s.metadata.name = Some(meta.0.into());
        s.metadata.namespace = Some(meta.1.into());
        s.metadata.labels = Some([("owner".into(), "helm".into()), ("name".into(), meta.2.into())].into());
        s.data = Some([("release".into(), k8s_openapi::ByteString(b64.into_bytes()))].into());
        s
    }

    pub(crate) fn payload(name: &str, ns: &str, version: i64) -> Value {
        serde_json::json!({"name": name, "namespace": ns, "version": version, "info": {"status": "deployed"},
            "chart": {"metadata": {"name": "c", "version": "1.0.0"}}, "manifest": ""})
    }

    #[test]
    fn decodes_a_release_from_its_secret() {
        let s = helm_secret(
            ("sh.helm.release.v1.cilium.v3", "kube-system", "cilium"),
            &payload("cilium", "kube-system", 3),
            true,
        );
        let r = decode(&s).unwrap();
        assert_eq!(
            (r.name.as_str(), r.namespace.as_str(), r.revision, r.chart.as_str()),
            ("cilium", "kube-system", 3, "c")
        );
    }

    #[test]
    fn spoofed_releases_are_dropped() {
        // A tenant's Secret claiming to be kube-system/cilium at revision 99999.
        let spoof = payload("cilium", "kube-system", 99999);
        assert!(decode(&helm_secret(("sh.helm.release.v1.cilium.v99999", "tenant", "cilium"), &spoof, true)).is_err());
        // The payload's name disagrees with the label.
        let other = payload("cilium", "tenant", 2);
        assert!(decode(&helm_secret(("sh.helm.release.v1.app.v2", "tenant", "app"), &other, true)).is_err());
        // Not Helm's Secret name for that revision.
        assert!(
            decode(&helm_secret(("cilium-fake", "tenant", "cilium"), &payload("cilium", "tenant", 2), false)).is_err()
        );
        // A label that is not a release name (it would land in a label selector).
        assert!(
            decode(&helm_secret(("sh.helm.release.v1.a,b.v1", "t", "a,b"), &payload("a,b", "t", 1), false)).is_err()
        );

        // A same-named release elsewhere never hides the real one.
        let real = decode(&helm_secret(
            ("sh.helm.release.v1.cilium.v3", "kube-system", "cilium"),
            &payload("cilium", "kube-system", 3),
            true,
        ))
        .unwrap();
        let theirs = decode(&helm_secret(
            ("sh.helm.release.v1.cilium.v99999", "tenant", "cilium"),
            &payload("cilium", "tenant", 99999),
            true,
        ))
        .unwrap();
        let kept = latest_revisions(vec![theirs, real]);
        assert_eq!(kept.len(), 2);
        let sys = kept.iter().find(|r| r.namespace == "kube-system").unwrap();
        assert_eq!(sys.revision, 3);
    }

    #[test]
    fn keeps_the_newest_revision() {
        let rel = |v| {
            decode(&helm_secret(
                (&format!("sh.helm.release.v1.web.v{v}"), "apps", "web"),
                &payload("web", "apps", v),
                false,
            ))
            .unwrap()
        };
        let kept = latest_revisions(vec![rel(4), rel(5)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].revision, 5);
    }

    #[test]
    fn gzip_bombs_are_refused() {
        // Highly compressible: a JSON string just over the cap.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"{\"name\":\"").unwrap();
        let zeros = vec![b'a'; 1 << 20];
        for _ in 0..33 {
            gz.write_all(&zeros).unwrap();
        }
        gz.write_all(b"\"}").unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        let mut s = helm_secret(("sh.helm.release.v1.x.v1", "t", "x"), &payload("x", "t", 1), false);
        s.data = Some([("release".into(), k8s_openapi::ByteString(b64.into_bytes()))].into());
        let err = decode(&s).err().expect("refused").to_string();
        assert!(err.contains("32 MiB"), "{err}");
    }

    #[test]
    fn rendered_secrets_are_masked() {
        let manifest = "---\n# Source: c/templates/cm.yaml\napiVersion: v1\nkind: ConfigMap\ndata:\n  a: visible\n---\n# Source: c/templates/secret.yaml\napiVersion: v1\nkind: Secret\nmetadata:\n  name: s\nstringData:\n  config: example-value\ndata:\n  token: ZXhhbXBsZQ==\n";
        let masked = mask_manifest(manifest);
        assert!(!masked.contains("example-value") && !masked.contains("ZXhhbXBsZQ"), "{masked}");
        assert!(masked.contains("config:") && masked.contains("a: visible"));
        assert!(
            masked.contains("# Source: c/templates/secret.yaml") && masked.contains("# Source: c/templates/cm.yaml")
        );
        assert_eq!(mask_manifest("kind: ConfigMap\n"), "kind: ConfigMap\n");
        // Unparsable but a Secret: hidden whole.
        assert!(!mask_manifest("kind: Secret\ndata: [x: {\n  p: example-value\n").contains("example-value"));
        assert!(!mask_manifest("kind: \"Secret\" # x\ndata: [x: {\n  p: example-value\n").contains("example-value"));
    }

    /// `--- # Source: …` separates documents like `---`: read as one document with the one
    /// before, a Secret's values used to be shown.
    #[test]
    fn a_separator_with_a_comment_still_splits() {
        let manifest = "apiVersion: v1\nkind: ConfigMap\ndata:\n  a: visible\n--- # Source: c/templates/secret.yaml\napiVersion: v1\nkind: Secret\nstringData:\n  config: example-value\n";
        let masked = mask_manifest(manifest);
        assert!(!masked.contains("example-value"), "{masked}");
        assert!(masked.contains("a: visible") && masked.contains("--- # Source: c/templates/secret.yaml"));
    }

    #[test]
    fn one_cluster_failing_keeps_the_others() {
        let web =
            decode(&helm_secret(("sh.helm.release.v1.web.v1", "ns", "web"), &payload("web", "ns", 1), false)).unwrap();
        let (ok, errors) =
            merge_clusters(vec![("a".into(), Ok(vec![web])), ("b".into(), Err(anyhow::anyhow!("forbidden")))]);
        assert_eq!(ok.unwrap().len(), 1);
        assert_eq!(errors, ["b: forbidden"]);
        let (all_failed, errors) = merge_clusters(vec![("b".into(), Err(anyhow::anyhow!("forbidden")))]);
        assert_eq!(all_failed.err().as_deref(), Some("forbidden"));
        assert!(errors.is_empty());
    }

    #[test]
    fn masks_secret_looking_values() {
        let mut v = serde_json::json!({
            "db": {"password": "example", "host": "pg", "port": 5432},
            "auth": {"apiKey": "abc", "publicKey": "pub"},
            "list": [{"token": "t"}],
            "mongodb": {"auth": {"passwords": ["p1", "p2"], "usernames": ["u1"]}},
            "sasl": {"client": {"passwords": ["p1", {"secret": "s", "name": "n"}]}},
        });
        mask_secrets(&mut v);
        assert_eq!(v["mongodb"]["auth"]["passwords"], serde_json::json!(["••••••", "••••••"]));
        assert_eq!(v["mongodb"]["auth"]["usernames"][0], "u1");
        assert_eq!(v["sasl"]["client"]["passwords"][0], "••••••");
        assert_eq!(v["sasl"]["client"]["passwords"][1]["secret"], "••••••");
        assert_eq!(v["sasl"]["client"]["passwords"][1]["name"], "n");
        assert_eq!(v["db"]["password"], "••••••");
        assert_eq!(v["db"]["host"], "pg");
        assert_eq!(v["db"]["port"], 5432);
        assert_eq!(v["auth"]["apiKey"], "••••••");
        assert_eq!(v["auth"]["publicKey"], "pub");
        assert_eq!(v["list"][0]["token"], "••••••");
    }

    #[test]
    fn masks_secret_env_values() {
        let mut v = serde_json::json!({
            "extraEnv": [
                {"name": "DB_PASSWORD", "value": "hunter2"},
                {"name": "api-key", "value": 1234},
                {"name": "DB_HOST", "value": "pg"},
                {"name": "DB_TOKEN", "valueFrom": {"secretKeyRef": {"name": "db", "key": "token"}}},
            ],
        });
        mask_secrets(&mut v);
        assert_eq!(v["extraEnv"][0]["value"], "••••••");
        assert_eq!(v["extraEnv"][0]["name"], "DB_PASSWORD");
        assert_eq!(v["extraEnv"][1]["value"], "••••••");
        assert_eq!(v["extraEnv"][2]["value"], "pg");
        assert_eq!(v["extraEnv"][3]["valueFrom"]["secretKeyRef"]["name"], "db");
    }

    /// The list's label-only rows take the same identity rules as `decode`.
    #[test]
    fn stubs_check_identity() {
        let meta = |secret: &str, labels: &[(&str, &str)]| ObjectMeta {
            name: Some(secret.into()),
            namespace: Some("kube-system".into()),
            labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
            ..Default::default()
        };
        let ok = meta(
            "sh.helm.release.v1.cilium.v3",
            &[("name", "cilium"), ("version", "3"), ("status", "deployed"), ("modifiedAt", "1700000000")],
        );
        let r = stub(&ok).unwrap();
        assert_eq!((r.name.as_str(), r.namespace.as_str(), r.revision), ("cilium", "kube-system", 3));
        assert_eq!(r.status, "deployed");
        assert_eq!(r.updated.map(jiff::Timestamp::as_second), Some(1_700_000_000));
        assert!(!r.loaded);
        // A Secret not named the way Helm names it, a revision that does not match, a bad name.
        assert!(stub(&meta("cilium", &[("name", "cilium"), ("version", "3")])).is_none());
        assert!(stub(&meta("sh.helm.release.v1.cilium.v3", &[("name", "cilium"), ("version", "4")])).is_none());
        assert!(stub(&meta("sh.helm.release.v1.a,b.v1", &[("name", "a,b"), ("version", "1")])).is_none());
    }
}
