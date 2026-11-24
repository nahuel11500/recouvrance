//! Certificates failing, for the Problems scan: cert-manager Certificates not Ready or not
//! renewed (judged from their own status, `cert_manager::judge`), and `kubernetes.io/tls`
//! Secrets whose certificate expired or ends soon.
//!
//! The Secrets are listed from the API server's cache, `type=kubernetes.io/tls` only, and
//! each one's certificate is read as the list is parsed: its two dates are kept, its key is
//! skipped. Certificates change rarely, so that list is read again at most every
//! `SECRETS_EVERY` per cluster and namespace.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use kube::Client;
use serde::{Deserialize, Deserializer};

use super::{Problem, Severity};
use crate::cert_manager::{self, time};
use crate::k8s::{self, Kind};

/// The cert-manager Certificates of `namespace` (every one when None) that fail: one list
/// from the API server's cache, without any Secret.
pub async fn certificates(client: Client, kind: &Kind, namespace: Option<String>) -> anyhow::Result<Vec<Problem>> {
    let items =
        k8s::lists::list_lean_cached::<cert_manager::LeanCertificate>(client, kind.ar.clone(), namespace).await?;
    let now = Timestamp::now();
    Ok(items
        .iter()
        .filter_map(|c| {
            let (severity, reason) = cert_manager::judge(c, now)?;
            Some(Problem {
                severity,
                kind: kind.kubectl_name(),
                label: kind.ar.kind.clone(),
                namespace: c.metadata.namespace.clone(),
                name: c.metadata.name.clone()?,
                reason,
                age: time(c.metadata.creation_timestamp.as_deref()),
            })
        })
        .collect())
}

/// How long a read of the TLS Secrets stands.
const SECRETS_EVERY: Duration = Duration::from_secs(600);

/// A window at most this long before its end counts as ending soon, for a certificate that
/// lives long enough (see `judge_tls`).
const SOON: i64 = 14 * 86400;

/// The dates of a Secret's certificate (the leaf of its `tls.crt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    pub not_before: Option<Timestamp>,
    pub not_after: Option<Timestamp>,
}

/// What is kept of a `kubernetes.io/tls` Secret.
#[derive(Clone, Debug)]
struct TlsCert {
    namespace: Option<String>,
    name: String,
    created: Option<Timestamp>,
    /// A cert-manager Certificate issued it: that Certificate is judged instead.
    managed: bool,
    validity: Validity,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanTlsSecret {
    metadata: SecretMeta,
    #[serde(deserialize_with = "k8s::null_default")]
    data: TlsData,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SecretMeta {
    name: Option<String>,
    namespace: Option<String>,
    creation_timestamp: Option<String>,
    #[serde(deserialize_with = "k8s::null_default")]
    annotations: SecretAnnotations,
}

/// Only the annotation that names the Certificate: the others (kubectl's last applied
/// configuration may hold the whole Secret) are skipped.
#[derive(Deserialize, Default)]
struct SecretAnnotations {
    #[serde(rename = "cert-manager.io/certificate-name")]
    certificate: Option<String>,
}

/// Only `tls.crt`, decoded as it is parsed: the key and every other entry are skipped.
#[derive(Deserialize, Default)]
struct TlsData {
    #[serde(rename = "tls.crt", default, deserialize_with = "leaf")]
    crt: Option<Validity>,
}

fn leaf<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Validity>, D::Error> {
    use base64::Engine as _;
    let text: Option<String> = Option::deserialize(d)?;
    Ok(text.and_then(|t| {
        let pem = base64::engine::general_purpose::STANDARD.decode(t.trim()).ok()?;
        let cert = crate::certs::parse_pem(&pem).into_iter().next()?;
        Some(Validity { not_before: cert.not_before, not_after: cert.not_after })
    }))
}

/// Judge a Secret's certificate at `now`. Expired is critical. Not valid yet (more than 5
/// minutes ahead: a clock skew) is a warning. Ending soon is a warning once less than
/// `SOON`, or a third of its lifetime, is left: whatever renews a certificate (cert-manager
/// at two thirds) has done it by then, so a short-lived one renewed on time is never
/// flagged. Critical under 3 days, or a tenth of its lifetime.
pub fn judge_tls(v: Validity, now: Timestamp) -> Option<(Severity, String)> {
    let end = v.not_after?;
    if end <= now {
        return Some((Severity::Critical, format!("certificate expired {}", crate::certs::ago_text(end, now))));
    }
    if let Some(start) = v.not_before.filter(|t| t.as_second() - now.as_second() > 300) {
        return Some((
            Severity::Warning,
            format!("certificate not valid until {}", start.strftime("%Y-%m-%d %H:%M UTC")),
        ));
    }
    let left = end.as_second() - now.as_second();
    // Unknown start: judged as a long-lived certificate.
    let lifetime = v.not_before.map_or(i64::MAX, |s| end.as_second() - s.as_second()).max(1);
    if left >= SOON.min(lifetime / 3) {
        return None;
    }
    let severity =
        if left < cert_manager::CRITICAL_LEFT.min(lifetime / 10) { Severity::Critical } else { Severity::Warning };
    Some((severity, format!("certificate expires in {}", crate::certs::days_text(crate::certs::days_left(end, now)))))
}

type Reads = HashMap<(String, Option<String>), (Instant, Arc<Vec<TlsCert>>)>;

/// The last read of each cluster's (and namespace's) TLS Secrets.
static READS: LazyLock<Mutex<Reads>> = LazyLock::new(Default::default);

/// The TLS Secrets of `namespace`, from a read less than `SECRETS_EVERY` old (and made since
/// `judged_since`) of `cluster` when there is one, else listed again.
async fn tls_certs(
    client: Client,
    kind: &Kind,
    namespace: Option<String>,
    cluster: Option<&str>,
    judged_since: Option<Instant>,
) -> anyhow::Result<Arc<Vec<TlsCert>>> {
    let key = cluster.map(|c| (c.to_string(), namespace.clone()));
    if let Some(key) = &key {
        let reads = READS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, certs)) = reads.get(key)
            && at.elapsed() < SECRETS_EVERY
            && judged_since.is_none_or(|since| *at >= since)
        {
            return Ok(certs.clone());
        }
    }
    let at = Instant::now();
    let fields = "type=kubernetes.io/tls".to_string();
    let items =
        k8s::lists::list_lean_cached_fields::<LeanTlsSecret>(client, kind.ar.clone(), namespace, fields).await?;
    let certs: Arc<Vec<TlsCert>> = Arc::new(
        items
            .into_iter()
            .filter_map(|s| {
                Some(TlsCert {
                    name: s.metadata.name?,
                    namespace: s.metadata.namespace,
                    created: time(s.metadata.creation_timestamp.as_deref()),
                    managed: s.metadata.annotations.certificate.is_some_and(|c| !c.is_empty()),
                    validity: s.data.crt?,
                })
            })
            .collect(),
    );
    if let Some(key) = key {
        let mut reads = READS.lock().unwrap_or_else(PoisonError::into_inner);
        // Reads too old to serve go: scopes nobody scans any more are not kept.
        reads.retain(|_, (at, _)| at.elapsed() < SECRETS_EVERY);
        reads.insert(key, (at, certs.clone()));
    }
    Ok(certs)
}

/// The TLS Secrets of `namespace` (every one when None) whose certificate expired or ends
/// soon. Those a cert-manager Certificate issued are left to it when the cluster serves
/// Certificates (`with_cert_manager`). A user who may not list Secrets gets nothing, not a
/// skipped check: many may not, and the scan would never read as complete for them.
pub async fn tls_secrets(
    client: Client,
    kind: &Kind,
    namespace: Option<String>,
    cluster: Option<&str>,
    judged_since: Option<Instant>,
    with_cert_manager: bool,
) -> anyhow::Result<Vec<Problem>> {
    let certs = match tls_certs(client, kind, namespace, cluster, judged_since).await {
        Ok(certs) => certs,
        Err(e) if k8s::api_status(&e) == Some(403) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(flagged(&certs, with_cert_manager, Timestamp::now())
        .map(|(c, severity, reason)| Problem {
            severity,
            kind: kind.kubectl_name(),
            label: kind.ar.kind.clone(),
            namespace: c.namespace.clone(),
            name: c.name.clone(),
            reason,
            age: c.created,
        })
        .collect())
}

/// The certificates `judge_tls` flags at `now`, but those a cert-manager Certificate issued
/// when the cluster serves Certificates: that Certificate says more (why it isn't renewed).
fn flagged(
    certs: &[TlsCert],
    with_cert_manager: bool,
    now: Timestamp,
) -> impl Iterator<Item = (&TlsCert, Severity, String)> {
    certs.iter().filter(move |c| !(with_cert_manager && c.managed)).filter_map(move |c| {
        let (severity, reason) = judge_tls(c.validity, now)?;
        Some((c, severity, reason))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86400;

    fn ts(secs: i64) -> Option<Timestamp> {
        Timestamp::from_second(NOW + secs).ok()
    }

    fn now() -> Timestamp {
        ts(0).unwrap()
    }

    fn validity(start: i64, end: i64) -> Validity {
        Validity { not_before: ts(start), not_after: ts(end) }
    }

    #[test]
    fn judges_secrets() {
        // 90 days, 60 left: fine; 10 left: renewal failed.
        assert_eq!(judge_tls(validity(-30 * DAY, 60 * DAY), now()), None);
        assert_eq!(
            judge_tls(validity(-80 * DAY, 10 * DAY), now()),
            Some((Severity::Warning, "certificate expires in 10 days".into()))
        );
        assert_eq!(judge_tls(validity(-88 * DAY, 2 * DAY), now()).map(|j| j.0), Some(Severity::Critical));
        assert_eq!(
            judge_tls(validity(-91 * DAY, -3 * 3600), now()),
            Some((Severity::Critical, "certificate expired less than a day ago".into()))
        );
        // A day-long certificate half way: renewed in time by whatever issues it.
        assert_eq!(judge_tls(validity(-12 * 3600, 12 * 3600), now()), None);
        assert!(
            judge_tls(validity(DAY, 90 * DAY), now()).is_some_and(|j| j.1.starts_with("certificate not valid until"))
        );
        // A minute of clock skew is nothing.
        assert_eq!(judge_tls(validity(60, 90 * DAY), now()), None);
        assert_eq!(judge_tls(Validity { not_before: None, not_after: None }, now()), None);
    }

    #[test]
    fn reads_only_the_certificate_of_a_secret() {
        use base64::Engine as _;
        let b64 = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
        let pem = include_str!("../../assets/test-cert.pem");
        let secret = |crt: String| {
            serde_json::json!({
                "metadata": {"name": "web-tls", "namespace": "a",
                    "annotations": {"cert-manager.io/certificate-name": "web",
                        "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"tls.key\":\"c2VjcmV0\"}}"}},
                "type": "kubernetes.io/tls",
                "data": {"tls.crt": crt, "tls.key": "c2VjcmV0"},
            })
        };
        let lean: LeanTlsSecret = serde_json::from_value(secret(b64(pem))).unwrap();
        assert_eq!(lean.metadata.annotations.certificate.as_deref(), Some("web"));
        let info = crate::certs::parse_pem(pem.as_bytes()).remove(0);
        assert_eq!(lean.data.crt, Some(Validity { not_before: info.not_before, not_after: info.not_after }));
        // No certificate in tls.crt: nothing to judge.
        let lean: LeanTlsSecret = serde_json::from_value(secret(b64("not a certificate"))).unwrap();
        assert_eq!(lean.data.crt, None);
        let empty: LeanTlsSecret =
            serde_json::from_value(serde_json::json!({"metadata": {"name": "x", "annotations": null}, "data": null}))
                .unwrap();
        assert_eq!(empty.data.crt, None);
    }

    /// A cluster with an expired Certificate and a Ready one, and two TLS Secrets: one the
    /// Ready Certificate issued, one made by hand. Records what it was asked for; with
    /// `.1`, it refuses to list Secrets.
    #[derive(Clone, Default)]
    struct Cluster(Arc<Mutex<Vec<String>>>, bool);

    impl tower::Service<http::Request<kube::client::Body>> for Cluster {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            use base64::Engine as _;
            let (path, query) = (req.uri().path().to_string(), req.uri().query().unwrap_or_default().to_string());
            self.0.lock().unwrap().push(format!("{path}?{query}"));
            let old = "2020-01-01T00:00:00Z";
            let list = |items: Vec<serde_json::Value>| serde_json::json!({"kind": "List", "apiVersion": "v1", "metadata": {"resourceVersion": "1"}, "items": items});
            let cert = |name: &str, ready: &str, not_after: &str| {
                serde_json::json!({"metadata": {"name": name, "namespace": "a", "creationTimestamp": old,
                        "managedFields": [{"manager": "cert-manager"}]},
                    "spec": {"secretName": format!("{name}-tls"), "issuerRef": {"name": "le", "kind": "ClusterIssuer"}},
                    "status": {"conditions": [{"type": "Ready", "status": ready, "reason": "Expired",
                        "message": "Certificate expired", "lastTransitionTime": old}],
                        "notAfter": not_after, "renewalTime": not_after}})
            };
            let crt = base64::engine::general_purpose::STANDARD.encode(include_str!("../../assets/test-cert.pem"));
            let secret = |name: &str, issued_by: Option<&str>| {
                serde_json::json!({"metadata": {"name": name, "namespace": "a", "creationTimestamp": old,
                        "annotations": issued_by.map(|c| serde_json::json!({"cert-manager.io/certificate-name": c}))},
                    "type": "kubernetes.io/tls", "data": {"tls.crt": crt, "tls.key": "c2VjcmV0"}})
            };
            let (code, body) = match path.as_str() {
                "/apis/cert-manager.io/v1/certificates" => (
                    200,
                    list(vec![
                        cert("gone", "False", "2021-01-01T00:00:00Z"),
                        cert("web", "True", "2999-01-01T00:00:00Z"),
                    ]),
                ),
                "/api/v1/secrets" if !self.1 => {
                    (200, list(vec![secret("web-tls", Some("web")), secret("manual", None)]))
                }
                _ => (
                    403,
                    serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "message": "forbidden", "reason": "Forbidden", "code": 403}),
                ),
            };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    fn kinds() -> Vec<Kind> {
        vec![Kind::fake(cert_manager::GROUP, "Certificate"), Kind::fake("", "Secret")]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scans_certificates_and_tls_secrets() {
        let cluster = Cluster::default();
        let client = Client::new(cluster.clone(), "default");
        // A context of its own: the reads kept between scans are this test's alone.
        let names = (None, Some("certificates-test"));
        let scan = super::super::run_all(super::super::jobs(&client, &kinds(), &[], names, None)).await;
        assert!(scan.skipped.is_empty());
        let found: Vec<(&str, &str, Severity)> =
            scan.problems.iter().map(|p| (p.kind.as_str(), p.name.as_str(), p.severity)).collect();
        // The test certificate is valid for years: no Secret is flagged; the expired
        // Certificate is.
        assert_eq!(found, [("certificates.cert-manager.io", "gone", Severity::Critical)]);
        assert!(scan.problems[0].reason.starts_with("certificate expired"), "{}", scan.problems[0].reason);
        let asked = cluster.0.lock().unwrap().clone();
        let secrets: Vec<&String> = asked.iter().filter(|a| a.starts_with("/api/v1/secrets")).collect();
        // TLS Secrets only, from the API server's cache.
        assert_eq!(secrets.len(), 1);
        assert!(secrets[0].contains("fieldSelector=type%3Dkubernetes.io%2Ftls"), "{}", secrets[0]);
        assert!(secrets[0].contains("resourceVersion=0"), "{}", secrets[0]);

        // The next scan within 10 minutes reuses the Secrets read; one asking for findings
        // from now on reads them again.
        let _ = super::super::run_all(super::super::jobs(&client, &kinds(), &[], names, None)).await;
        let count = || cluster.0.lock().unwrap().iter().filter(|a| a.starts_with("/api/v1/secrets")).count();
        assert_eq!(count(), 1);
        let _ = super::super::run_all(super::super::jobs(&client, &kinds(), &[], names, Some(Instant::now()))).await;
        assert_eq!(count(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secrets_that_may_not_be_listed_are_not_a_skipped_check() {
        let cluster = Cluster(Arc::default(), true);
        let client = Client::new(cluster, "default");
        let scan = super::super::run_all(super::super::jobs(&client, &kinds(), &[], (None, None), None)).await;
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
        assert_eq!(scan.problems.len(), 1);
    }

    #[test]
    fn a_secret_a_certificate_issued_is_left_to_it() {
        let soon = Validity { not_before: ts(-80 * DAY), not_after: ts(10 * DAY) };
        let cert = |managed| TlsCert { namespace: None, name: "x".into(), created: None, managed, validity: soon };
        let certs = [cert(true), cert(false)];
        let names = |with_cert_manager| {
            flagged(&certs, with_cert_manager, now()).map(|(c, ..)| c.managed).collect::<Vec<bool>>()
        };
        assert_eq!(names(true), Vec::from([false]));
        // A cluster without cert-manager: the annotation alone says nothing.
        assert_eq!(names(false), Vec::from([true, false]));
    }

    proptest! {
        /// Expired is critical; more than `SOON` left is never flagged; a certificate is
        /// flagged ending soon only once less than a third of its lifetime is left, so one
        /// renewed at two thirds of its life never is.
        #[test]
        fn ending_soon_follows_the_lifetime(lifetime in 60i64..800 * DAY, age_permille in 0i64..1100) {
            let start = -(lifetime * age_permille / 1000);
            let end = start + lifetime;
            let judged = judge_tls(validity(start, end), now());
            if end <= 0 {
                prop_assert_eq!(judged.map(|j| j.0), Some(Severity::Critical));
            } else {
                // Flagged exactly when less than both are left.
                prop_assert_eq!(judged.is_some(), end < SOON.min(lifetime / 3));
            }
        }
    }
}
