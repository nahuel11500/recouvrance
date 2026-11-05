//! The filter box of the Timeline and Problems pages: `ctx:`, `ns:` and `kind:` terms, the
//! rest searched as text. Every term must match, ignoring case.

#[derive(Default, Debug, PartialEq, Eq)]
pub struct PageFilter {
    pub context: Vec<String>,
    pub namespace: Vec<String>,
    pub kind: Vec<String>,
    pub text: Vec<String>,
}

impl PageFilter {
    pub fn parse(input: &str) -> Self {
        let mut f = Self::default();
        for word in input.split_whitespace() {
            let word = word.to_lowercase();
            let term = match word.split_once(':') {
                Some(("ctx" | "context", v)) if !v.is_empty() => Some((&mut f.context, v.to_string())),
                Some(("ns" | "namespace", v)) if !v.is_empty() => Some((&mut f.namespace, v.to_string())),
                Some(("kind", v)) if !v.is_empty() => Some((&mut f.kind, v.to_string())),
                _ => None,
            };
            match term {
                Some((terms, v)) => terms.push(v),
                None => f.text.push(word),
            }
        }
        f
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Whether a row with this context, namespace and kind matches, `text` being what its
    /// words are searched in. A kind term matches part of the kind or its plural
    /// (`kind:configmap`, `kind:configmaps`, `kind:config`); a namespace term part of the
    /// namespace.
    pub fn matches(&self, context: &str, namespace: Option<&str>, kind: &str, text: &str) -> bool {
        let kind = kind.to_lowercase();
        let kind_ok = |k: &String| kind.contains(k.as_str()) || format!("{kind}s") == *k;
        let ns = namespace.unwrap_or_default().to_lowercase();
        let context = context.to_lowercase();
        if !(self.context.iter().all(|c| context.contains(c.as_str()))
            && self.namespace.iter().all(|n| ns.contains(n.as_str()))
            && self.kind.iter().all(kind_ok))
        {
            return false;
        }
        if self.text.is_empty() {
            return true;
        }
        let text = text.to_lowercase();
        self.text.iter().all(|t| text.contains(t.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::PageFilter;

    #[test]
    fn terms_and_text() {
        let f = PageFilter::parse("ctx:Prod ns:shop kind:ConfigMap web");
        assert_eq!(f.context, ["prod"]);
        assert_eq!(f.namespace, ["shop"]);
        assert_eq!(f.kind, ["configmap"]);
        assert_eq!(f.text, ["web"]);
        // A term with nothing after the colon, or an unknown one, is text.
        assert_eq!(PageFilter::parse("ns: a:b").text, ["ns:", "a:b"]);
        assert!(PageFilter::parse("  ").is_empty());
    }

    #[test]
    fn every_term_must_match() {
        let ok = |f: &str| PageFilter::parse(f).matches("prod-eu", Some("shop"), "ConfigMap", "prod-eu web changed");
        assert!(ok(""));
        assert!(ok("ctx:prod ns:shop kind:configmaps WEB"));
        assert!(ok("kind:config"));
        assert!(!ok("kind:service"));
        assert!(!ok("ns:other"));
        assert!(!ok("web nothing"));
        // No namespace: only a filter without `ns:` keeps it.
        let f = PageFilter::parse("ns:shop");
        assert!(!f.matches("prod", None, "Node", "n1"));
    }
}
