//! The prune verdict: what a sync with prune would delete, read as advice. Prune stays the
//! user's choice; this only says, before the choice, which of three cases applies:
//!
//! - **not needed**: the sync would delete nothing;
//! - **recommended**: it would delete only resources removed from git that hold no data;
//! - **data would be deleted**: at least one of them holds data (a volume, a StatefulSet, a
//!   namespace, a database run by an operator), said per component with what it would lose.
//!
//! The resources are those Argo CD marks to prune (in the cluster, no longer in git), named by
//! component (see `components::component_of`).

use super::components::component_of;
use super::diagnosis::Res;

/// Components named in a recommended prune's line, at most; the others counted.
const NAMED: usize = 5;

/// Kinds whose deletion loses data: (API group, kind, one, several). A kind is matched with
/// its group; a resource said without a group (an older status) matches by kind alone, except
/// for a kind as common as `Cluster`.
const DATA_KINDS: [(&str, &str, &str, &str); 15] = [
    ("", "Namespace", "namespace", "namespaces"),
    ("acid.zalan.do", "postgresql", "postgres database", "postgres databases"),
    ("postgresql.cnpg.io", "Cluster", "postgres database", "postgres databases"),
    ("pxc.percona.com", "PerconaXtraDBCluster", "MySQL database", "MySQL databases"),
    ("mysql.oracle.com", "InnoDBCluster", "MySQL database", "MySQL databases"),
    ("psmdb.percona.com", "PerconaServerMongoDB", "MongoDB database", "MongoDB databases"),
    ("mongodbcommunity.mongodb.com", "MongoDBCommunity", "MongoDB database", "MongoDB databases"),
    ("clickhouse.altinity.com", "ClickHouseInstallation", "ClickHouse database", "ClickHouse databases"),
    ("elasticsearch.k8s.elastic.co", "Elasticsearch", "Elasticsearch cluster", "Elasticsearch clusters"),
    ("kafka.strimzi.io", "Kafka", "Kafka cluster", "Kafka clusters"),
    ("rabbitmq.com", "RabbitmqCluster", "RabbitMQ cluster", "RabbitMQ clusters"),
    ("databases.spotahome.com", "RedisFailover", "Redis database", "Redis databases"),
    ("apps", "StatefulSet", "StatefulSet", "StatefulSets"),
    ("", "PersistentVolumeClaim", "volume", "volumes"),
    ("", "PersistentVolume", "volume", "volumes"),
];

/// Where `r` is in [`DATA_KINDS`], when its deletion loses data.
fn data_kind(r: &Res) -> Option<usize> {
    DATA_KINDS.iter().position(|(group, kind, _, _)| {
        r.kind == *kind && (r.group == *group || (r.group.is_empty() && *kind != "Cluster"))
    })
}

/// Which of the three cases applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to prune.
    NotNeeded,
    /// Only resources without data, removed from git: their components.
    Recommended(Vec<String>),
    /// Some resources hold data: each component that would lose some, with what
    /// (`StatefulSet, 3 volumes`); then the components pruned without data.
    DataLost { losses: Vec<(String, String)>, others: Vec<String> },
}

/// The verdict on pruning `pruned`, the resources a sync with prune would delete.
pub fn verdict(pruned: &[Res]) -> Verdict {
    if pruned.is_empty() {
        return Verdict::NotNeeded;
    }
    // Each component in the order first met, with how many of each data kind it holds.
    let mut components: Vec<(String, [usize; DATA_KINDS.len()])> = Vec::new();
    for r in pruned {
        let name = component_of(&r.name);
        let at = components.iter().position(|(n, _)| *n == name).unwrap_or_else(|| {
            components.push((name, [0; DATA_KINDS.len()]));
            components.len() - 1
        });
        if let Some(k) = data_kind(r) {
            components[at].1[k] += 1;
        }
    }
    let (with, without): (Vec<_>, Vec<_>) =
        components.into_iter().partition(|(_, counts)| counts.iter().any(|n| *n > 0));
    let others: Vec<String> = without.into_iter().map(|(n, _)| n).collect();
    if with.is_empty() {
        return Verdict::Recommended(others);
    }
    let losses = with
        .into_iter()
        .map(|(name, counts)| {
            // Two kinds may say the same (a claim and a volume): said once, counted together.
            let mut said: Vec<(&str, &str, usize)> = Vec::new();
            for (k, n) in counts.iter().enumerate().filter(|(_, n)| **n > 0) {
                let (_, _, one, many) = DATA_KINDS[k];
                match said.iter_mut().find(|(o, _, _)| *o == one) {
                    Some(s) => s.2 += n,
                    None => said.push((one, many, *n)),
                }
            }
            let what: Vec<String> = said
                .into_iter()
                .map(|(one, many, n)| if n == 1 { one.to_string() } else { format!("{n} {many}") })
                .collect();
            (name, what.join(", "))
        })
        .collect();
    Verdict::DataLost { losses, others }
}

/// Some names, the first few, then how many more.
fn some(names: &[String]) -> String {
    let mut out: Vec<String> = names.iter().take(NAMED).cloned().collect();
    if names.len() > NAMED {
        out.push(format!("+{} more", names.len() - NAMED));
    }
    out.join(", ")
}

impl Verdict {
    /// Its headline: `Prune: not needed`, `Prune: recommended`, `Prune: data would be deleted`.
    pub fn title(&self) -> &'static str {
        match self {
            Self::NotNeeded => "Prune: not needed",
            Self::Recommended(_) => "Prune: recommended",
            Self::DataLost { .. } => "Prune: data would be deleted",
        }
    }

    /// What follows the headline, a line each: why, or what each component would lose.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::NotNeeded => vec!["Nothing to prune.".into()],
            Self::Recommended(names) => vec![format!("Prunes {}, removed from git.", some(names))],
            Self::DataLost { losses, others } => {
                let mut out: Vec<String> = losses.iter().map(|(n, what)| format!("{n}: {what}")).collect();
                if !others.is_empty() {
                    out.push(format!("Also prunes {}, without data.", some(others)));
                }
                out
            }
        }
    }

    /// What to make of it, on hover: it is advice, and the choice stays the user's.
    pub fn advice(&self) -> &'static str {
        match self {
            Self::NotNeeded => "Every resource in the cluster is still in git: a sync with prune deletes nothing.",
            Self::Recommended(_) => {
                "These resources were removed from git and hold no data: a sync with Prune ticked deletes them. \
                 Without prune they stay in the cluster, out of sync. Advice only: Prune stays your choice."
            }
            Self::DataLost { .. } => {
                "A resource removed from git holds data (a volume, a StatefulSet, a namespace or a database): a sync \
                 with Prune ticked deletes it, and its data with it. Back the data up, or keep the resource in git, \
                 before pruning. Advice only: Prune stays your choice."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(group: &str, kind: &str, name: &str) -> Res {
        Res {
            group: group.into(),
            kind: kind.into(),
            name: name.into(),
            namespace: "shop".into(),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_to_prune_is_not_needed() {
        let v = verdict(&[]);
        assert_eq!(v, Verdict::NotNeeded);
        assert_eq!(v.title(), "Prune: not needed");
        assert_eq!(v.lines(), ["Nothing to prune."]);
    }

    #[test]
    fn resources_without_data_are_recommended_by_component() {
        let pruned = [
            res("apps", "Deployment", "proxy-rotation"),
            res("autoscaling", "HorizontalPodAutoscaler", "proxy-rotation"),
            res("", "ConfigMap", "proxy-rotation"),
            res("batch", "CronJob", "daily-clean-v1"),
        ];
        let v = verdict(&pruned);
        assert_eq!(v, Verdict::Recommended(vec!["proxy-rotation".into(), "daily-clean-v1".into()]));
        assert_eq!(v.lines(), ["Prunes proxy-rotation, daily-clean-v1, removed from git."]);
    }

    #[test]
    fn data_says_what_each_component_loses() {
        let pruned = [
            res("apps", "StatefulSet", "search-warm"),
            res("", "Service", "search-warm"),
            res("", "PersistentVolumeClaim", "data-search-warm-0"),
            res("", "PersistentVolumeClaim", "data-search-warm-1"),
            res("", "PersistentVolumeClaim", "data-search-warm-2"),
            res("acid.zalan.do", "postgresql", "reports"),
            res("", "PersistentVolumeClaim", "pgdata-reports-0"),
            res("apps", "Deployment", "old-worker"),
        ];
        let v = verdict(&pruned);
        assert_eq!(v.title(), "Prune: data would be deleted");
        assert_eq!(
            v.lines(),
            [
                "search-warm: StatefulSet, 3 volumes",
                "reports: postgres database, volume",
                "Also prunes old-worker, without data."
            ]
        );
    }

    /// Any one resource holding data turns the verdict red, whatever else is pruned.
    #[test]
    fn one_data_kind_is_enough() {
        for (group, kind) in [
            ("", "PersistentVolumeClaim"),
            ("", "PersistentVolume"),
            ("apps", "StatefulSet"),
            ("", "Namespace"),
            ("acid.zalan.do", "postgresql"),
            ("postgresql.cnpg.io", "Cluster"),
        ] {
            let pruned = [res("", "ConfigMap", "a"), res(group, kind, "b")];
            assert!(matches!(verdict(&pruned), Verdict::DataLost { .. }), "{group}/{kind}");
        }
    }

    /// A kind is read with its group: a `Cluster` of another API holds no database.
    #[test]
    fn kinds_are_matched_with_their_group() {
        let holds_data = |r: &Res| data_kind(r).is_some();
        assert!(!holds_data(&res("cluster.x-k8s.io", "Cluster", "a")));
        assert!(!holds_data(&res("", "Cluster", "a")), "a bare Cluster is too common to be a database");
        assert!(holds_data(&res("", "StatefulSet", "a")), "said without its group");
        assert!(!holds_data(&res("apps", "Deployment", "a")));
    }

    #[test]
    fn many_components_are_cut_short() {
        let pruned: Vec<Res> = (0..8).map(|i| res("", "ConfigMap", &format!("c{i}x"))).collect();
        assert_eq!(verdict(&pruned).lines(), ["Prunes c0x, c1x, c2x, c3x, c4x, +3 more, removed from git."]);
    }
}
