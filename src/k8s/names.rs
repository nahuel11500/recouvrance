//! Checks for names that end up in API URLs. kube formats `namespaces/{ns}/…/{name}`
//! without encoding, so a `/`, `?` or `%` in a name would reach another path or query.

/// A namespace: a DNS-1123 label.
pub fn validate_namespace(ns: &str) -> Result<(), String> {
    if is_dns_label(ns) {
        Ok(())
    } else {
        Err(format!(
            "invalid namespace {ns:?}: lowercase letters, digits and '-', at most 63 characters \
             (omit `namespace` for all namespaces)"
        ))
    }
}

/// A container name: a DNS-1123 label.
pub fn validate_container(name: &str) -> Result<(), String> {
    if is_dns_label(name) {
        Ok(())
    } else {
        Err(format!("invalid container name {name:?}: lowercase letters, digits and '-', at most 63 characters"))
    }
}

/// An object name. Most kinds use DNS-1123 subdomains; RBAC objects also allow ':'
/// (`system:node`) and some allow uppercase, so this is a safe superset.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 253
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '@' | '+'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid name {name:?}: letters, digits and - . _ : @ + only (no '/', '?', '%', '..' or spaces), \
             at most 253 characters"
        ))
    }
}

/// A Kind like `Pod` or `HelmRelease`, as used in field selectors.
pub fn validate_kind_name(kind: &str) -> Result<(), String> {
    if !kind.is_empty() && kind.len() <= 63 && kind.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!("invalid kind {kind:?}: letters and digits only, like Pod"))
    }
}

fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    let inner = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty() && b.len() <= 63 && inner(&b[0]) && inner(&b[b.len() - 1]) && b.iter().all(|c| inner(c) || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces() {
        for ok in ["default", "kube-system", "a", "team-1", &"a".repeat(63)] {
            assert!(validate_namespace(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "prod/secrets/db?",
            "prod/secrets",
            "kube%2Dsystem",
            "Default",
            "-a",
            "a-",
            "a.b",
            "a b",
            "a\n",
            "..",
            &"a".repeat(64),
        ] {
            assert!(validate_namespace(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names() {
        for ok in ["web", "kube-root-ca.crt", "system:node", "system:controller:job-controller", "v1.apps", "Ab_c"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "x/proxy/pods",
            "node-1/proxy",
            "x?dryRun=All",
            "x#y",
            "%2e%2e",
            "a b",
            "a\tb",
            "a\u{0}",
            "..",
            "a..b",
            "x,involvedObject.namespace=kube-system",
            "x=y",
            &"a".repeat(254),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn containers_and_kinds() {
        assert!(validate_container("app").is_ok());
        assert!(validate_container("app&previous=true").is_err());
        assert!(validate_container("App").is_err());
        assert!(validate_kind_name("Pod").is_ok());
        assert!(validate_kind_name("Pod,involvedObject.name=x").is_err());
    }
}
