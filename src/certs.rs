//! Decode PEM certificates (TLS secrets, CA bundles) for display.

use x509_cert::der::{Decode as _, oid::db::rfc5280::ID_CE_SUBJECT_ALT_NAME};
use x509_cert::ext::pkix::SubjectAltName;
use x509_cert::ext::pkix::name::GeneralName;

pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    pub sans: Vec<String>,
    pub not_before: Option<jiff::Timestamp>,
    pub not_after: Option<jiff::Timestamp>,
    pub is_ca: bool,
}

impl CertInfo {
    /// Its validity starts after `now` (a clock skew when it was issued): clients reject it.
    pub fn not_yet_valid(&self, now: jiff::Timestamp) -> bool {
        self.not_before.is_some_and(|t| t > now)
    }
}

/// Every certificate in a PEM bundle; empty when the text holds none.
pub fn parse_pem(text: &[u8]) -> Vec<CertInfo> {
    let Ok(blocks) = pem::parse_many(text) else { return Vec::new() };
    blocks.iter().filter(|b| b.tag() == "CERTIFICATE").filter_map(|b| parse_der(b.contents())).collect()
}

fn parse_der(der: &[u8]) -> Option<CertInfo> {
    let cert = x509_cert::Certificate::from_der(der).ok()?;
    let tbs = cert.tbs_certificate();
    let ts = |t: &x509_cert::time::Time| jiff::Timestamp::from_second(t.to_unix_duration().as_secs() as i64).ok();
    let mut sans = Vec::new();
    let mut is_ca = false;
    for ext in tbs.extensions().into_iter().flatten() {
        if ext.extn_id == ID_CE_SUBJECT_ALT_NAME {
            if let Ok(san) = SubjectAltName::from_der(ext.extn_value.as_bytes()) {
                sans.extend(san.0.iter().filter_map(|n| match n {
                    GeneralName::DnsName(d) => Some(d.to_string()),
                    GeneralName::IpAddress(ip) => match ip.as_bytes() {
                        [a, b, c, d] => Some(format!("{a}.{b}.{c}.{d}")),
                        _ => None,
                    },
                    GeneralName::Rfc822Name(e) => Some(e.to_string()),
                    _ => None,
                }));
            }
        } else if ext.extn_id == x509_cert::der::oid::db::rfc5280::ID_CE_BASIC_CONSTRAINTS
            && let Ok(bc) = x509_cert::ext::pkix::BasicConstraints::from_der(ext.extn_value.as_bytes())
        {
            is_ca = bc.ca;
        }
    }
    Some(CertInfo {
        subject: tbs.subject().to_string(),
        issuer: tbs.issuer().to_string(),
        sans,
        not_before: ts(&tbs.validity().not_before),
        not_after: ts(&tbs.validity().not_after),
        is_ca,
    })
}

#[cfg(test)]
mod tests {
    // Self-signed, CN=test.example, SAN test.example + 10.0.0.1, valid ten years.
    const PEM: &str = include_str!("../assets/test-cert.pem");

    #[test]
    fn parses_certificate() {
        let certs = super::parse_pem(PEM.as_bytes());
        assert_eq!(certs.len(), 1);
        let c = &certs[0];
        assert!(c.subject.contains("CN=test.example"), "{}", c.subject);
        assert!(c.sans.contains(&"test.example".to_string()));
        assert!(c.sans.contains(&"10.0.0.1".to_string()));
        assert!(c.not_after.is_some_and(|t| t > c.not_before.unwrap()));
        assert!(super::parse_pem(b"not a cert").is_empty());
    }

    #[test]
    fn not_yet_valid_before_not_before() {
        let c = super::parse_pem(PEM.as_bytes()).remove(0);
        let start = c.not_before.unwrap();
        assert!(c.not_yet_valid(start - jiff::SignedDuration::from_hours(1)));
        assert!(!c.not_yet_valid(start));
        assert!(!c.not_yet_valid(start + jiff::SignedDuration::from_hours(1)));
    }
}
