//! cert-manager's Certificates: whether one fails to be issued or renewed, from its own
//! status (no Secret is read), and why, from its last CertificateRequest.

use jiff::Timestamp;
use kube::Client;
use kube::discovery::ApiResource;
use serde::Deserialize;

use crate::k8s;
use crate::problems::Severity;

pub const GROUP: &str = "cert-manager.io";

/// Seconds a Certificate may be not Ready before it counts: a new one, or one whose Secret
/// was just deleted, is issued again in a moment.
const NOT_READY_GRACE: i64 = 600;

/// Seconds a renewal may take past its `renewalTime`: an ACME challenge can take minutes.
const RENEWAL_GRACE: i64 = 3600;

/// Seconds left under which a failing certificate is critical.
pub const CRITICAL_LEFT: i64 = 3 * 86400;

/// One of a status's conditions.
#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub last_transition_time: Option<String>,
}

impl Condition {
    /// Its message, else its reason, on one short line; None when it has neither.
    fn why(&self) -> Option<String> {
        let text = self.message.as_deref().filter(|m| !m.trim().is_empty()).or(self.reason.as_deref())?;
        Some(crate::external_secrets::shorten(text)).filter(|t| !t.is_empty())
    }
}

fn condition<'a>(conditions: &'a [Condition], ty: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == ty)
}

pub fn time(t: Option<&str>) -> Option<Timestamp> {
    t.and_then(|t| t.parse().ok())
}

/// What is read of a Certificate: its name, Secret and the status cert-manager keeps. Its
/// spec's other fields and managedFields are skipped as the list is parsed.
#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct LeanCertificate {
    pub metadata: Meta,
    pub spec: Spec,
    pub status: Status,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Meta {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub creation_timestamp: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Spec {
    pub secret_name: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    #[serde(deserialize_with = "k8s::null_default")]
    pub conditions: Vec<Condition>,
    pub not_before: Option<String>,
    pub not_after: Option<String>,
    pub renewal_time: Option<String>,
    pub last_failure_time: Option<String>,
    pub failed_issuance_attempts: Option<i64>,
}

impl LeanCertificate {
    pub fn ready(&self) -> Option<&Condition> {
        condition(&self.status.conditions, "Ready")
    }

    /// Why its last issuance failed: the `Issuing` condition cert-manager leaves False with
    /// reason `Failed`.
    fn issuing_failed(&self) -> Option<String> {
        condition(&self.status.conditions, "Issuing")
            .filter(|c| c.status == "False" && c.reason.as_deref() == Some("Failed"))
            .and_then(Condition::why)
    }
}

/// Judge a Certificate at `now`; None while cert-manager keeps it valid. Expired is
/// critical. Not Ready for more than `NOT_READY_GRACE`, or not renewed `RENEWAL_GRACE`
/// after its `renewalTime`, is a warning, critical within `CRITICAL_LEFT` of its end. Its
/// own `renewalTime` says when renewal is due, so a short-lived certificate (a day, a week)
/// is judged as fairly as a 90-day one.
pub fn judge(cert: &LeanCertificate, now: Timestamp) -> Option<(Severity, String)> {
    let not_after = time(cert.status.not_after.as_deref());
    let ready = cert.ready().filter(|r| r.status != "True");
    let why = cert.issuing_failed().or_else(|| ready.and_then(Condition::why));
    let with_why = |head: String| match &why {
        Some(w) => format!("{head}: {w}"),
        None => head,
    };
    if let Some(end) = not_after.filter(|t| *t <= now) {
        return Some((
            Severity::Critical,
            with_why(format!("certificate expired {}", crate::certs::ago_text(end, now))),
        ));
    }
    let left = not_after.map(|t| t.as_second() - now.as_second());
    let severity = if left.is_some_and(|l| l < CRITICAL_LEFT) { Severity::Critical } else { Severity::Warning };
    let ends = not_after
        .map_or_default(|t| format!(", expires in {}", crate::certs::days_text(crate::certs::days_left(t, now))));
    if let Some(r) = ready {
        let since =
            time(r.last_transition_time.as_deref()).or_else(|| time(cert.metadata.creation_timestamp.as_deref()));
        if since.is_none_or(|t| now.as_second() - t.as_second() > NOT_READY_GRACE) {
            let head = match r.reason.as_deref().filter(|r| !r.is_empty()) {
                Some(reason) => format!("not ready ({reason}){ends}"),
                None => format!("not ready{ends}"),
            };
            return Some((severity, with_why(head)));
        }
    }
    let due = time(cert.status.renewal_time.as_deref())?;
    let late = now.as_second() - due.as_second();
    (late > RENEWAL_GRACE)
        .then(|| (severity, with_why(format!("not renewed, due {} ago{ends}", k8s::duration_text(late)))))
}

/// A Certificate's status, from its object on its page.
pub fn of_value(obj: &serde_json::Value) -> Option<LeanCertificate> {
    LeanCertificate::deserialize(obj).ok()
}

/// What is read of a CertificateRequest: who owns it, its revision, and its conditions.
/// Its CSR and certificate are skipped as the list is parsed.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanRequest {
    metadata: RequestMeta,
    status: RequestStatus,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RequestMeta {
    name: Option<String>,
    creation_timestamp: Option<String>,
    #[serde(deserialize_with = "k8s::null_default")]
    annotations: RequestAnnotations,
    #[serde(deserialize_with = "k8s::null_default")]
    owner_references: Vec<Owner>,
}

#[derive(Deserialize, Default)]
struct RequestAnnotations {
    #[serde(rename = "cert-manager.io/certificate-revision")]
    revision: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Owner {
    kind: String,
    uid: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RequestStatus {
    #[serde(deserialize_with = "k8s::null_default")]
    conditions: Vec<Condition>,
    failure_time: Option<String>,
}

/// A Certificate's last CertificateRequest: the one of its latest revision.
#[derive(Clone, Debug, Default)]
pub struct LastRequest {
    pub name: String,
    pub created: Option<Timestamp>,
    pub revision: Option<u64>,
    /// Why it failed (denied, invalid, or not issued), None while it is fine or pending.
    pub failed: Option<String>,
    pub failure_time: Option<Timestamp>,
    /// Its Ready condition's status ("True", "False"), when it has one.
    pub ready: Option<String>,
}

/// Why a request failed: denied, invalid, or Ready False with reason `Failed`.
fn request_failure(conditions: &[Condition]) -> Option<String> {
    let bad = |ty: &str| condition(conditions, ty).filter(|c| c.status == "True");
    let failed =
        condition(conditions, "Ready").filter(|c| c.status == "False" && c.reason.as_deref() == Some("Failed"));
    let c = bad("Denied").or_else(|| bad("InvalidRequest")).or(failed)?;
    Some(c.why().unwrap_or_else(|| c.type_.clone()))
}

/// The request of the highest revision among those the Certificate `uid` owns (the newest
/// when revisions tie or are missing).
fn latest(requests: Vec<LeanRequest>, uid: &str) -> Option<LastRequest> {
    requests
        .into_iter()
        .filter(|r| r.metadata.owner_references.iter().any(|o| o.kind == "Certificate" && o.uid == uid))
        .map(|r| LastRequest {
            name: r.metadata.name.unwrap_or_default(),
            created: time(r.metadata.creation_timestamp.as_deref()),
            revision: r.metadata.annotations.revision.and_then(|v| v.parse().ok()),
            failed: request_failure(&r.status.conditions),
            failure_time: time(r.status.failure_time.as_deref()),
            ready: condition(&r.status.conditions, "Ready").map(|c| c.status.clone()),
        })
        .max_by_key(|r| (r.revision, r.created))
}

/// The last CertificateRequest of the Certificate `obj`: one list of its namespace's
/// requests, from the API server's cache. None when it has none.
pub async fn last_request(client: Client, obj: &serde_json::Value) -> anyhow::Result<Option<LastRequest>> {
    let ptr = |p: &str| obj.pointer(p).and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
    let (api_version, ns, uid) = (ptr("/apiVersion"), ptr("/metadata/namespace"), ptr("/metadata/uid"));
    let version = api_version.split_once('/').map_or("v1", |(_, v)| v).to_string();
    let ar = ApiResource {
        group: GROUP.into(),
        api_version: format!("{GROUP}/{version}"),
        version,
        kind: "CertificateRequest".into(),
        plural: "certificaterequests".into(),
    };
    let requests = k8s::lists::list_lean_cached::<LeanRequest>(client, ar, Some(ns)).await?;
    Ok(latest(requests, &uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NOW: i64 = 1_800_000_000;

    fn at(secs: i64) -> String {
        Timestamp::from_second(NOW + secs).unwrap().to_string()
    }

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).unwrap()
    }

    fn cert(status: serde_json::Value) -> LeanCertificate {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": "web", "namespace": "a", "creationTimestamp": at(-100 * 86400)},
            "spec": {"secretName": "web-tls", "dnsNames": ["example.com"]},
            "status": status,
        }))
        .unwrap()
    }

    fn ready(status: &str, reason: &str, message: &str, since: i64) -> serde_json::Value {
        serde_json::json!({"type": "Ready", "status": status, "reason": reason, "message": message,
            "lastTransitionTime": at(since)})
    }

    const DAY: i64 = 86400;

    #[test]
    fn a_valid_certificate_is_fine() {
        let c = cert(serde_json::json!({
            "conditions": [ready("True", "Ready", "Certificate is up to date and has not expired", -DAY)],
            "notBefore": at(-30 * DAY), "notAfter": at(60 * DAY), "renewalTime": at(30 * DAY),
        }));
        assert_eq!(judge(&c, now()), None);
    }

    #[test]
    fn renewal_overdue_says_why() {
        let c = cert(serde_json::json!({
            "conditions": [
                ready("True", "Ready", "Certificate is up to date and has not expired", -60 * DAY),
                {"type": "Issuing", "status": "False", "reason": "Failed",
                 "message": "The certificate request has failed to complete and will be retried: ACME challenge failed"},
            ],
            "notBefore": at(-80 * DAY), "notAfter": at(10 * DAY), "renewalTime": at(-20 * DAY),
        }));
        assert_eq!(
            judge(&c, now()),
            Some((
                Severity::Warning,
                "not renewed, due 20d ago, expires in 10 days: The certificate request has failed to complete \
                 and will be retried: ACME challenge failed"
                    .into()
            ))
        );
        // Within its grace, a renewal is still on its way.
        let soon = cert(serde_json::json!({
            "conditions": [ready("True", "Ready", "", -DAY)],
            "notAfter": at(30 * DAY), "renewalTime": at(-600),
        }));
        assert_eq!(judge(&soon, now()), None);
    }

    #[test]
    fn not_ready_after_its_grace() {
        let status = |since| {
            serde_json::json!({
                "conditions": [ready("False", "DoesNotExist", "Issuing certificate as Secret does not exist", since)],
            })
        };
        assert_eq!(judge(&cert(status(-60)), now()), None);
        assert_eq!(
            judge(&cert(status(-3600)), now()),
            Some((Severity::Warning, "not ready (DoesNotExist): Issuing certificate as Secret does not exist".into()))
        );
    }

    #[test]
    fn expired_is_critical() {
        let c = cert(serde_json::json!({
            "conditions": [ready("False", "Expired", "Certificate expired on Sun, 01 Jan 2023", -2 * DAY)],
            "notAfter": at(-2 * DAY - 3600), "renewalTime": at(-30 * DAY),
        }));
        assert_eq!(
            judge(&c, now()),
            Some((
                Severity::Critical,
                "certificate expired 2 days ago: Certificate expired on Sun, 01 Jan 2023".into()
            ))
        );
    }

    #[test]
    fn the_last_request_says_why_it_failed() {
        let request = |name: &str, uid: &str, revision: Option<&str>, conditions: serde_json::Value| -> LeanRequest {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": name, "creationTimestamp": at(-DAY),
                    "annotations": revision.map(|r| serde_json::json!({"cert-manager.io/certificate-revision": r})),
                    "ownerReferences": [{"kind": "Certificate", "uid": uid, "name": "web"}]},
                "spec": {"request": "LS0tLS1CRUdJTi..."},
                "status": {"conditions": conditions},
            }))
            .unwrap()
        };
        let failed = serde_json::json!([{"type": "Ready", "status": "False", "reason": "Failed",
            "message": "Failed to wait for order resource to become ready"}]);
        let denied = serde_json::json!([{"type": "Denied", "status": "True", "reason": "Policy", "message": ""}]);
        let ok = serde_json::json!([{"type": "Ready", "status": "True", "reason": "Issued"}]);
        let requests = vec![
            request("web-1", "u", Some("1"), ok.clone()),
            request("web-3", "u", Some("3"), failed),
            request("web-2", "u", Some("2"), ok),
            request("other-9", "v", Some("9"), denied.clone()),
        ];
        let last = latest(requests, "u").unwrap();
        assert_eq!(last.name, "web-3");
        assert_eq!(last.failed.as_deref(), Some("Failed to wait for order resource to become ready"));
        let last = latest(vec![request("x-1", "u", None, denied)], "u").unwrap();
        assert_eq!(last.failed.as_deref(), Some("Policy"));
        assert!(latest(Vec::new(), "u").is_none());
    }

    proptest! {
        /// A Ready certificate is flagged exactly when it expired or its renewal is more
        /// than `RENEWAL_GRACE` late, however long it lives: a short-lived one renewed on
        /// time is never flagged.
        #[test]
        fn ready_certificates_are_judged_by_their_renewal(
            lifetime in 3600i64..400 * DAY,
            age_permille in 0i64..1200,
        ) {
            let age = lifetime * age_permille / 1000;
            let start = -age;
            let end = start + lifetime;
            // cert-manager's default: renew at 2/3 of the lifetime.
            let renewal = start + lifetime * 2 / 3;
            let c = cert(serde_json::json!({
                "conditions": [ready("True", "Ready", "", start)],
                "notBefore": at(start), "notAfter": at(end), "renewalTime": at(renewal),
            }));
            let judged = judge(&c, now());
            let expected = end <= 0 || -renewal > RENEWAL_GRACE;
            prop_assert_eq!(judged.is_some(), expected);
            if end <= 0 {
                prop_assert_eq!(judged.unwrap().0, Severity::Critical);
            }
        }
    }
}
