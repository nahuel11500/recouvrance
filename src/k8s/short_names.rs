//! Short names for long context names, on screen only. The cloud CLIs name a context after
//! the whole cluster id (`arn:aws:eks:eu-west-1:123456789012:cluster/prod-eu`,
//! `gke_project_zone_prod-eu`), so names differ only at the end, the part a cut name loses.
//! A context shows its cluster name instead, with the region, then the account or project,
//! added only when two would read the same. Everything else (keys, settings, protection,
//! agent rules, kubectl lines, MCP answers) keeps the full name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock};

/// The short names of the kubeconfig's contexts, by full name. Only the ones that differ.
static SHORT: RwLock<Option<Arc<HashMap<String, String>>>> = RwLock::new(None);

/// Work the short names out again for this list of contexts (on launch and on each
/// kubeconfig reload): whether a short name is clear depends on the others.
pub fn set_contexts(contexts: &[String]) {
    let names = Arc::new(short_names(contexts));
    *SHORT.write().unwrap_or_else(PoisonError::into_inner) = Some(names);
}

/// What the screen shows for `context`: its short name, or the name itself when it has none.
pub fn short_name(context: &str) -> String {
    let names = SHORT.read().unwrap_or_else(PoisonError::into_inner).clone();
    names.and_then(|n| n.get(context).cloned()).unwrap_or_else(|| context.to_string())
}

/// An EKS context named after its cluster's ARN: (cluster name, region, account).
fn eks(context: &str) -> Option<(&str, &str, &str)> {
    let mut parts = context.splitn(6, ':');
    let (arn, partition, service) = (parts.next()?, parts.next()?, parts.next()?);
    let (region, account, resource) = (parts.next()?, parts.next()?, parts.next()?);
    let name = resource.strip_prefix("cluster/")?;
    let ok = arn == "arn"
        && partition.starts_with("aws")
        && service == "eks"
        && !region.is_empty()
        && !account.is_empty()
        && !name.is_empty()
        && !name.contains('/');
    ok.then_some((name, region, account))
}

/// A GKE context, `gke_<project>_<location>_<cluster>`: (cluster name, location, project).
/// None of the three can hold an underscore, so anything else is left alone.
fn gke(context: &str) -> Option<(&str, &str, &str)> {
    let mut parts = context.strip_prefix("gke_")?.split('_');
    let (project, location, name) = (parts.next()?, parts.next()?, parts.next()?);
    let ok = parts.next().is_none() && !project.is_empty() && !location.is_empty() && !name.is_empty();
    ok.then_some((name, location, project))
}

/// The names `context` can show, shortest first. The last is always the full name.
fn candidates(context: &str) -> Vec<String> {
    let Some((name, place, owner)) = eks(context).or_else(|| gke(context)) else {
        return vec![context.to_string()];
    };
    vec![name.to_string(), format!("{name} ({place})"), format!("{name} ({owner}, {place})"), context.to_string()]
}

/// The short name of each context whose short name differs from its own, for this list.
/// Each context starts at its shortest name; while two show the same text, or one shows
/// another context's full name, those that can show a longer one do. A full name is never
/// taken by another, so no two contexts end up showing the same text, whatever the order.
pub fn short_names(contexts: &[String]) -> HashMap<String, String> {
    let options: BTreeMap<&str, Vec<String>> = contexts.iter().map(|c| (c.as_str(), candidates(c))).collect();
    let full: HashSet<&str> = options.keys().copied().collect();
    let mut level: BTreeMap<&str, usize> = options.keys().map(|c| (*c, 0)).collect();
    loop {
        let mut uses: HashMap<&str, usize> = HashMap::new();
        for (c, l) in &level {
            *uses.entry(options[c][*l].as_str()).or_default() += 1;
        }
        let mut changed = false;
        for (c, l) in &mut level {
            let names = &options[c];
            let shown = names[*l].as_str();
            let unclear = uses[shown] > 1 || (shown != *c && full.contains(shown));
            if unclear && *l + 1 < names.len() {
                *l += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    level
        .into_iter()
        .filter_map(|(c, l)| {
            let shown = &options[c][l];
            (shown != c).then(|| (c.to_string(), shown.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::short_names;

    fn shown(all: &[&str]) -> Vec<String> {
        let all: Vec<String> = all.iter().map(ToString::to_string).collect();
        let short = short_names(&all);
        all.iter().map(|c| short.get(c).unwrap_or(c).clone()).collect()
    }

    #[test]
    fn a_lone_cloud_context_shows_its_cluster_name() {
        assert_eq!(shown(&["arn:aws:eks:eu-west-1:123456789012:cluster/prod-eu"]), ["prod-eu"]);
        assert_eq!(shown(&["arn:aws-cn:eks:cn-north-1:123456789012:cluster/prod"]), ["prod"]);
        assert_eq!(shown(&["gke_my-project_europe-west1-b_prod-eu"]), ["prod-eu"]);
    }

    #[test]
    fn other_names_are_left_alone() {
        let names = [
            "kind-kind",
            "prod",
            "arn:aws:iam::123456789012:role/admin",
            "arn:aws:eks:eu-west-1:123456789012:cluster/",
            "arn:aws:eks:eu-west-1:123456789012:nodegroup/prod/ng",
            "gke_project_zone",
            "gke_a_b_c_d",
            "gke__zone_name",
            "",
        ];
        assert_eq!(shown(&names), names);
    }

    /// Two clusters named alike in two regions show the region; in one region and two
    /// accounts, the account too. One with a name of its own stays short.
    #[test]
    fn alike_names_show_what_tells_them_apart() {
        assert_eq!(
            shown(&[
                "arn:aws:eks:eu-west-1:111:cluster/prod",
                "arn:aws:eks:us-east-1:111:cluster/prod",
                "arn:aws:eks:us-east-1:111:cluster/staging",
            ]),
            ["prod (eu-west-1)", "prod (us-east-1)", "staging"]
        );
        assert_eq!(
            shown(&["arn:aws:eks:eu-west-1:111:cluster/prod", "arn:aws:eks:eu-west-1:222:cluster/prod"]),
            ["prod (111, eu-west-1)", "prod (222, eu-west-1)"]
        );
        assert_eq!(
            shown(&["gke_a_europe-west1_prod", "gke_b_europe-west1_prod", "gke_b_us-east1_prod"]),
            ["prod (a, europe-west1)", "prod (b, europe-west1)", "prod (us-east1)"]
        );
    }

    /// A short name never reads as another context's own name.
    #[test]
    fn a_short_name_never_takes_another_contexts_name() {
        assert_eq!(shown(&["prod", "arn:aws:eks:eu-west-1:111:cluster/prod"]), ["prod", "prod (eu-west-1)"]);
        assert_eq!(
            shown(&["prod (eu-west-1)", "arn:aws:eks:eu-west-1:111:cluster/prod", "gke_p_eu-west-1_prod"]),
            ["prod (eu-west-1)", "prod (111, eu-west-1)", "prod (p, eu-west-1)"]
        );
    }

    /// Context names made of few pieces, so that short names clash often.
    fn context() -> impl Strategy<Value = String> {
        let piece = || prop::sample::select(vec!["a", "b", "a (r1)", "a (1, r1)", "r1", "1"]);
        prop_oneof![
            (prop::sample::select(vec!["r1", "r2"]), prop::sample::select(vec!["1", "2"]), piece())
                .prop_map(|(r, acct, n)| format!("arn:aws:eks:{r}:{acct}:cluster/{n}")),
            (prop::sample::select(vec!["1", "p"]), prop::sample::select(vec!["r1", "z"]), piece())
                .prop_map(|(p, z, n)| format!("gke_{p}_{z}_{n}")),
            piece().prop_map(String::from),
            "[a-z_:/() 1]{0,14}",
        ]
    }

    proptest! {
        /// Whatever the contexts: no two show the same text, each shows its own name or a
        /// one no longer that starts with its cluster name, and the order of the list changes nothing.
        #[test]
        fn short_names_are_unique_and_stable(all in prop::collection::btree_set(context(), 0..12), seed in any::<u8>()) {
            let all: Vec<String> = all.into_iter().collect();
            let short = short_names(&all);
            let shown: Vec<&String> = all.iter().map(|c| short.get(c).unwrap_or(c)).collect();
            prop_assert_eq!(shown.iter().collect::<HashSet<_>>().len(), all.len());
            for c in &all {
                if let Some(s) = short.get(c) {
                    prop_assert!(s.len() <= c.len(), "{s} for {c}");
                    let name = super::eks(c).or_else(|| super::gke(c)).map(|p| p.0);
                    prop_assert!(name.is_some_and(|n| s.starts_with(n)), "{s} for {c}");
                }
            }
            prop_assert!(short.keys().all(|c| all.contains(c)));
            let mut turned = all.clone();
            turned.rotate_left(usize::from(seed) % all.len().max(1));
            turned.reverse();
            prop_assert_eq!(short_names(&turned), short);
        }
    }
}
