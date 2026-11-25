//! The environments of the Argo CD page's list, worked out once per read of the feeds (off the
//! UI thread): each one's state, images, last sync and destination, the filter that keeps
//! them, and the lines of the list (project, type, environment, its Applications).

use std::collections::{HashMap, HashSet};

use crate::k8s::argo::environments::{self, EnvImage, EnvTypes};
use crate::k8s::argo::{self, FleetApp, Summary};
use crate::status::Tone;

/// Images shown on a row, at most; the others are counted, all named on hover.
pub const IMAGES_SHOWN: usize = 3;

/// What an environment's Applications are, at worst.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub out_of_sync: bool,
    /// Degraded (or Missing with auto-sync on).
    pub degraded: bool,
    /// Its last sync failed.
    pub failed: bool,
    /// An operation runs.
    pub running: bool,
}

impl State {
    pub fn of<'a>(apps: impl Iterator<Item = &'a Summary>) -> Self {
        let mut s = Self::default();
        for a in apps {
            s.out_of_sync |= a.sync == "OutOfSync";
            s.degraded |= argo::listed_health_tone(&a.health, a.automated) == Tone::Bad;
            s.failed |= argo::sync_failed(a);
            s.running |= a.in_progress
                || a.operation.as_ref().is_some_and(|(phase, _, _)| argo::operation_tone(phase) == Tone::Busy);
        }
        s
    }

    /// Worth a look: out of sync, Degraded or a sync that failed.
    pub fn needs_look(self) -> bool {
        self.out_of_sync || self.degraded || self.failed
    }
}

/// One environment of the list.
#[derive(Clone, Debug)]
pub struct Env {
    /// Its Applications (indexes in the fleet) in tree order, the root first, each with its
    /// depth under the root.
    pub members: Vec<(usize, usize)>,
    pub name: String,
    pub project: String,
    /// Its type, an index of the types (`EnvTypes`).
    pub kind: usize,
    pub tone: Tone,
    pub state: State,
    /// Its product's images all carry a version tag.
    pub on_tag: bool,
    /// Its images, the product's first (see `environments::differing`).
    pub images: Vec<(bool, EnvImage)>,
    /// When its last operation ended.
    pub last_sync: Option<jiff::Timestamp>,
    /// Where its workload deploys: Argo CD's name of the cluster, and the namespace.
    pub cluster: String,
    pub namespace: String,
    /// The Application of it that runs the most images (its workload), else its root.
    pub main: usize,
    /// What the search looks in, in lowercase: its Applications' names, namespaces, clusters,
    /// project, server and image tags.
    pub search: String,
}

impl Env {
    pub fn root(&self) -> usize {
        self.members[0].0
    }
}

/// The color of a group: red when one of them is, amber when one is on its way, green when
/// all are.
pub fn group_tone(tones: impl Iterator<Item = Tone>) -> Tone {
    let tones: Vec<Tone> = tones.collect();
    if tones.contains(&Tone::Bad) {
        Tone::Bad
    } else if tones.contains(&Tone::Busy) {
        Tone::Busy
    } else if !tones.is_empty() && tones.iter().all(|t| *t == Tone::Good) {
        Tone::Good
    } else {
        Tone::Done
    }
}

/// A tag as shown: each commit sha in it cut to 7 characters (the shared
/// `argo::short_revision`), so `main-77b324b4…` reads `main-77b324b`.
pub fn short_tag(tag: &str) -> String {
    argo::short_revision(tag)
}

/// The environments of `apps`, from the parents found (`parents`) and a tree order of them
/// (`order`), typed by `types`.
pub fn environments(
    apps: &[FleetApp],
    parents: &[Option<usize>],
    order: &[(usize, usize)],
    types: &EnvTypes,
) -> Vec<Env> {
    let roots = environments::roots(parents);
    let groups = environments::group(&roots, order);
    let images: Vec<Vec<EnvImage>> = groups
        .iter()
        .map(|m| environments::images(m.iter().flat_map(|(i, _)| apps[*i].app.images.iter().map(String::as_str))))
        .collect();
    // The product's images: those whose tags differ between the project's environments.
    let mut by_project: HashMap<&str, Vec<usize>> = HashMap::new();
    for (e, m) in groups.iter().enumerate() {
        by_project.entry(apps[m[0].0].app.project.as_str()).or_default().push(e);
    }
    let mut product: Vec<HashSet<String>> = vec![HashSet::new(); groups.len()];
    for envs in by_project.values() {
        let all: Vec<&[EnvImage]> = envs.iter().map(|&e| images[e].as_slice()).collect();
        let differing = environments::differing(&all);
        for &e in envs {
            product[e].clone_from(&differing);
        }
    }
    groups
        .into_iter()
        .zip(images)
        .zip(product)
        .map(|((members, images), product)| {
            let root = &apps[members[0].0];
            let summaries = || members.iter().map(|(i, _)| &apps[*i].app);
            let on_tag = environments::on_tag(&images, &product);
            let main = members
                .iter()
                .map(|(i, _)| *i)
                .max_by_key(|&i| (apps[i].app.images.len(), std::cmp::Reverse(i)))
                .filter(|&i| !apps[i].app.images.is_empty())
                .unwrap_or(members[0].0);
            let last_sync =
                summaries().filter_map(|a| a.operation.as_ref()?.2.as_deref()?.parse::<jiff::Timestamp>().ok()).max();
            let images: Vec<(bool, EnvImage)> =
                environments::product_first(&images, &product).into_iter().map(|(own, i)| (own, i.clone())).collect();
            let mut search: Vec<String> = Vec::new();
            for (i, _) in &members {
                let a = &apps[*i];
                search.extend([a.app.name.clone(), a.app.dest_namespace.clone(), a.cluster.clone()]);
            }
            search.extend([root.app.project.clone(), root.server.name()]);
            for (_, i) in &images {
                search.push(i.component.clone());
                for t in &i.tags {
                    search.extend([t.clone(), short_tag(t)]);
                }
            }
            Env {
                name: root.app.name.clone(),
                project: root.app.project.clone(),
                kind: types.of(&root.app.name, on_tag),
                tone: group_tone(summaries().map(argo::attention)),
                state: State::of(summaries()),
                on_tag,
                images,
                last_sync,
                cluster: apps[main].cluster.clone(),
                namespace: apps[main].app.dest_namespace.clone(),
                main,
                search: search.join(" ").to_lowercase(),
                members,
            }
        })
        .collect()
}

/// The line above the list: how many environments (`kept` of them, when filtered) and
/// Applications, and which servers' first list is still read (`reading`): never "0
/// environments" while one is (#1076).
pub fn summary(envs: usize, kept: Option<usize>, apps: usize, reading: &[String]) -> String {
    let apps_text = crate::plural::count(apps, "Application");
    let counted = match kept {
        None => format!("{} · {apps_text}", crate::plural::count(envs, "environment")),
        Some(kept) => format!("{kept} of {} · {apps_text}", crate::plural::count(envs, "environment")),
    };
    if reading.is_empty() {
        return counted;
    }
    let reading = format!("Reading Applications from {}…", reading.join(", "));
    if apps == 0 { reading } else { format!("{counted} · {reading}") }
}

/// Whether a column says nothing for this list: the same value on (nearly) every row.
pub fn uniform<'a>(values: impl Iterator<Item = &'a str>) -> bool {
    let mut count: HashMap<&str, usize> = HashMap::new();
    let mut n = 0;
    for v in values {
        *count.entry(v).or_default() += 1;
        n += 1;
    }
    count.values().max().is_none_or(|&top| top * 20 >= n * 19)
}

// MARK: Filter

/// What the list keeps: an environment in any of the states picked (none picked: all), of
/// the project and type picked, a favorite when only those are asked, matching the search.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Filter {
    pub needs_look: bool,
    pub out_of_sync: bool,
    /// Degraded, or a sync that failed.
    pub degraded: bool,
    pub running: bool,
    pub favorites: bool,
    pub project: Option<String>,
    /// A type's name.
    pub kind: Option<String>,
    /// Only those whose product runs on version tags.
    pub on_tag: bool,
    /// In lowercase.
    pub query: String,
}

impl Filter {
    /// Whether it keeps `env`, of type `kind` (its name), a favorite or not; `labels` are its
    /// Applications' (a search word `key=value` is one of them).
    pub fn keeps(&self, env: &Env, kind: &str, favorite: bool, labels: &[&(String, String)]) -> bool {
        let s = env.state;
        let any_state = self.needs_look || self.out_of_sync || self.degraded || self.running;
        let state = !any_state
            || (self.needs_look && s.needs_look())
            || (self.out_of_sync && s.out_of_sync)
            || (self.degraded && (s.degraded || s.failed))
            || (self.running && s.running);
        state
            && (favorite || !self.favorites)
            && self.project.as_ref().is_none_or(|p| *p == env.project)
            && self.kind.as_ref().is_none_or(|k| k == kind)
            && (env.on_tag || !self.on_tag)
            && query_keeps(&self.query, &env.search, labels)
    }
}

/// Whether each word of the search (in lowercase) is in the searchable text, a word
/// `key=value` being a label one of its Applications has instead (`key=` any value of it).
pub fn query_keeps(query: &str, search: &str, labels: &[&(String, String)]) -> bool {
    query.split_whitespace().all(|word| match word.split_once('=') {
        Some((key, value)) if !key.is_empty() => {
            labels.iter().any(|(k, v)| k.to_lowercase() == key && (value.is_empty() || v.to_lowercase() == value))
        }
        _ => search.contains(word),
    })
}

// MARK: Lines

/// How the list groups its environments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroupBy {
    /// By project, then type.
    #[default]
    Type,
    /// By project.
    Project,
    /// Not grouped: by name.
    None,
}

impl GroupBy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Type => "Group: project, type",
            Self::Project => "Group: project",
            Self::None => "Not grouped",
        }
    }

    /// The headings an environment is under, outermost first.
    pub fn path(self, env: &Env, types: &EnvTypes) -> Vec<String> {
        match self {
            Self::Type => vec![env.project.clone(), types.name(env.kind).to_string()],
            Self::Project => vec![env.project.clone()],
            Self::None => Vec::new(),
        }
    }
}

/// A heading of the list: a project, or a type in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// What its fold is remembered by: the headings above and its own title.
    pub key: String,
    pub title: String,
    pub depth: usize,
    /// The environments the filter keeps under it, and how many of those need a look.
    pub envs: usize,
    pub looks: usize,
    pub tone: Tone,
}

/// A line of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// A heading, by index in the headings.
    Heading(usize),
    /// An environment, by index.
    Env(usize),
    /// One of the Applications of the environment (by index), at this position of its members.
    App(usize, usize),
}

/// What a heading's fold is kept under.
pub fn heading_key(path: &[String]) -> String {
    path.join(" / ")
}

/// The lines of the list: the environments `listed` (each with its headings, in the order
/// shown) the filter keeps (`keeps`, by environment), under their headings; nothing under a
/// folded heading (`folded`, by key); an expanded environment's Applications under it
/// (`expanded`, with how many it has: `members`). A heading with nothing kept under it is
/// left out.
pub fn lines(
    listed: &[(Vec<String>, usize)],
    keeps: &[bool],
    folded: &HashSet<String>,
    expanded: impl Fn(usize) -> Option<usize>,
    looks: impl Fn(usize) -> bool,
    tone: impl Fn(usize) -> Tone,
) -> (Vec<Line>, Vec<Heading>) {
    let mut out = Vec::new();
    let mut heads: Vec<Heading> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for (path, e) in listed {
        if !keeps.get(*e).copied().unwrap_or(false) {
            continue;
        }
        let same = open.iter().zip(path).take_while(|(h, title)| heads[**h].title == **title).count();
        open.truncate(same);
        while open.len() < path.len() {
            let depth = open.len();
            let hidden = open.iter().any(|&h| folded.contains(&heads[h].key));
            heads.push(Heading {
                key: heading_key(&path[..=depth]),
                title: path[depth].clone(),
                depth,
                envs: 0,
                looks: 0,
                tone: Tone::Done,
            });
            open.push(heads.len() - 1);
            if !hidden {
                out.push(Line::Heading(heads.len() - 1));
            }
        }
        for &h in &open {
            let head = &mut heads[h];
            head.envs += 1;
            head.looks += usize::from(looks(*e));
            head.tone = if head.envs == 1 { tone(*e) } else { group_tone([head.tone, tone(*e)].into_iter()) };
        }
        if open.iter().any(|&h| folded.contains(&heads[h].key)) {
            continue;
        }
        out.push(Line::Env(*e));
        if let Some(n) = expanded(*e) {
            out.extend((0..n).map(|m| Line::App(*e, m)));
        }
    }
    (out, heads)
}

/// An environment's Applications in the order its dashboard shows them: its workload (`main`)
/// first, the others in tree order, its root (the app of apps, whose cards list its
/// children) last.
pub fn section_order(env: &Env) -> Vec<usize> {
    let root = env.root();
    let mut out = vec![env.main];
    out.extend(env.members.iter().map(|(i, _)| *i).filter(|&i| i != env.main && i != root));
    if root != env.main {
        out.push(root);
    }
    out
}

/// An Application's name under its environment's: what follows `<environment>-`, else its own.
pub fn member_label<'a>(env: &str, app: &'a str) -> &'a str {
    app.strip_prefix(env).and_then(|rest| rest.strip_prefix('-')).filter(|r| !r.is_empty()).unwrap_or(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, project: &str, kind: usize, state: State) -> Env {
        Env {
            members: vec![(0, 0)],
            name: name.into(),
            project: project.into(),
            kind,
            tone: Tone::Good,
            state,
            on_tag: false,
            images: Vec::new(),
            last_sync: None,
            cluster: String::new(),
            namespace: String::new(),
            main: 0,
            search: name.into(),
        }
    }

    #[test]
    fn state_filters_add_up_and_the_others_narrow() {
        let oos = env("shop-prod-a", "shop", 0, State { out_of_sync: true, ..Default::default() });
        let failed = env("shop-prod-b", "shop", 0, State { failed: true, ..Default::default() });
        let running = env("data-acc", "data", 3, State { running: true, ..Default::default() });
        let fine = env("data-prod", "data", 0, State::default());
        let keeps = |f: &Filter, e: &Env| f.keeps(e, "prod", false, &[]);
        assert!([&oos, &failed, &running, &fine].iter().all(|e| keeps(&Filter::default(), e)));
        let look = Filter { needs_look: true, ..Default::default() };
        assert!(keeps(&look, &oos) && keeps(&look, &failed) && !keeps(&look, &running) && !keeps(&look, &fine));
        let either = Filter { degraded: true, running: true, ..Default::default() };
        assert!(keeps(&either, &failed) && keeps(&either, &running) && !keeps(&either, &oos));
        let shop = Filter { project: Some("shop".into()), ..look };
        assert!(keeps(&shop, &oos) && !keeps(&shop, &running));
        assert!(!Filter { kind: Some("staging".into()), ..Default::default() }.keeps(&fine, "prod", false, &[]));
        assert!(!Filter { on_tag: true, ..Default::default() }.keeps(&fine, "prod", false, &[]));
        let favorites = Filter { favorites: true, ..Default::default() };
        assert!(favorites.keeps(&fine, "prod", true, &[]) && !favorites.keeps(&fine, "prod", false, &[]));
        let search = |q: &str| Filter { query: q.into(), ..Default::default() };
        assert!(keeps(&search("prod a"), &oos) && !keeps(&search("prod c"), &oos));
        let label = ("team".to_string(), "Web".to_string());
        assert!(search("team=web").keeps(&fine, "prod", false, &[&label]));
        assert!(search("team=").keeps(&fine, "prod", false, &[&label]));
        assert!(!search("team=api").keeps(&fine, "prod", false, &[&label]));
    }

    #[test]
    fn environments_sit_under_their_project_and_type() {
        let p = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
        let listed =
            vec![(p("data", "prod"), 0), (p("shop", "prod"), 1), (p("shop", "prod"), 2), (p("shop", "acc"), 3)];
        let all = [true; 4];
        let none = HashSet::new();
        let (l, h) = lines(&listed, &all, &none, |e| (e == 2).then_some(2), |e| e == 3, |_| Tone::Good);
        let titles: Vec<&str> = h.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["data", "prod", "shop", "prod", "acc"]);
        assert_eq!(
            l,
            [
                Line::Heading(0),
                Line::Heading(1),
                Line::Env(0),
                Line::Heading(2),
                Line::Heading(3),
                Line::Env(1),
                Line::Env(2),
                Line::App(2, 0),
                Line::App(2, 1),
                Line::Heading(4),
                Line::Env(3)
            ]
        );
        assert_eq!((h[2].envs, h[2].looks, h[3].envs), (3, 1, 2));
        // Folded: the heading stays, what is under it goes. Filtered: no heading over nothing.
        let folded: HashSet<String> = [heading_key(&p("shop", "prod")[..1])].into();
        let (l, _) = lines(&listed, &all, &folded, |_| None, |_| false, |_| Tone::Good);
        assert_eq!(l, [Line::Heading(0), Line::Heading(1), Line::Env(0), Line::Heading(2)]);
        let (l, h) = lines(&listed, &[false, false, false, true], &none, |_| None, |_| false, |_| Tone::Good);
        assert_eq!(l, [Line::Heading(0), Line::Heading(1), Line::Env(3)]);
        assert_eq!(h.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["shop", "acc"]);
    }

    #[test]
    fn the_count_never_says_none_while_a_server_is_read() {
        let one = ["argocd.example.com".to_string()];
        assert_eq!(summary(0, None, 0, &one), "Reading Applications from argocd.example.com…");
        assert_eq!(summary(0, Some(0), 0, &one), "Reading Applications from argocd.example.com…");
        assert_eq!(
            summary(1, Some(0), 3, &one),
            "0 of 1 environment · 3 Applications · Reading Applications from argocd.example.com…"
        );
        assert_eq!(summary(2, None, 3, &[]), "2 environments · 3 Applications");
        assert_eq!(summary(2, Some(1), 3, &[]), "1 of 2 environments · 3 Applications");
    }

    #[test]
    fn the_workload_comes_first_and_the_root_last() {
        let mut e = env("shop-prod-a", "shop", 0, State::default());
        e.members = vec![(4, 0), (5, 1), (6, 1)];
        e.main = 6;
        assert_eq!(section_order(&e), [6, 5, 4]);
        e.main = 4;
        assert_eq!(section_order(&e), [4, 5, 6], "no workload: the root first");
        e.members = vec![(4, 0)];
        assert_eq!(section_order(&e), [4]);
    }

    #[test]
    fn a_member_is_named_after_its_environment() {
        assert_eq!(member_label("shop-prod-a", "shop-prod-a-workload"), "workload");
        assert_eq!(member_label("shop-prod-a", "shop-prod-a"), "shop-prod-a");
        assert_eq!(member_label("shop-prod-a", "shop-prod-ab"), "shop-prod-ab");
        assert_eq!(member_label("shop-prod-a", "other"), "other");
    }

    #[test]
    fn a_column_the_same_on_nearly_every_row_says_nothing() {
        assert!(uniform(std::iter::repeat_n("gra", 40).chain(["sbg"])));
        assert!(!uniform(std::iter::repeat_n("gra", 20).chain(std::iter::repeat_n("sbg", 5))));
        assert!(!uniform(["gra", "gra", "sbg"].into_iter()));
        assert!(uniform(std::iter::empty()));
    }

    #[test]
    fn a_group_is_as_red_as_its_worst() {
        assert_eq!(group_tone([Tone::Good, Tone::Bad, Tone::Busy].into_iter()), Tone::Bad);
        assert_eq!(group_tone([Tone::Good, Tone::Busy].into_iter()), Tone::Busy);
        assert_eq!(group_tone([Tone::Good, Tone::Good].into_iter()), Tone::Good);
        assert_eq!(group_tone([Tone::Good, Tone::Done].into_iter()), Tone::Done);
    }
}

/// Rules that hold for any list.
#[cfg(test)]
mod properties {
    use super::{Line, Tone, heading_key, lines};
    use proptest::prelude::*;
    use std::collections::HashSet;

    proptest! {
        /// With nothing folded, every environment kept is shown once and none other; each
        /// heading shown has something under it, and its count is what is under it.
        #[test]
        fn every_kept_environment_is_shown_once(
            paths in proptest::collection::vec((0u8..3, 0u8..3), 0..20),
            keeps in proptest::collection::vec(any::<bool>(), 20),
            fold in proptest::option::of((0u8..3, proptest::option::of(0u8..3))),
        ) {
            let mut listed: Vec<(Vec<String>, usize)> =
                paths.iter().enumerate().map(|(e, (p, t))| (vec![format!("p{p}"), format!("t{t}")], e)).collect();
            listed.sort();
            let keeps = &keeps[..listed.len()];
            let (l, h) = lines(&listed, keeps, &HashSet::new(), |_| None, |_| false, |_| Tone::Good);
            let mut shown: Vec<usize> = l.iter().filter_map(|l| if let Line::Env(e) = l { Some(*e) } else { None }).collect();
            shown.sort_unstable();
            let want: Vec<usize> = (0..listed.len()).filter(|&e| keeps[e]).collect();
            prop_assert_eq!(&shown, &want);
            prop_assert_eq!(h.iter().filter(|h| h.depth == 0).map(|h| h.envs).sum::<usize>(), want.len());
            // Folded: nothing under the heading, everything else as before.
            if let Some((p, t)) = fold {
                let mut path = vec![format!("p{p}")];
                path.extend(t.map(|t| format!("t{t}")));
                let folded: HashSet<String> = [heading_key(&path)].into();
                let (lf, _) = lines(&listed, keeps, &folded, |_| None, |_| false, |_| Tone::Good);
                let under = |e: usize| listed.iter().find(|(_, x)| *x == e).is_some_and(|(full, _)| full.starts_with(&path));
                let shown: Vec<usize> = lf.iter().filter_map(|l| if let Line::Env(e) = l { Some(*e) } else { None }).collect();
                prop_assert!(shown.iter().all(|&e| !under(e)));
                prop_assert_eq!(shown.len(), want.iter().filter(|&&e| !under(e)).count());
            }
        }
    }
}
