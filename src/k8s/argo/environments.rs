//! Environments: a root Application (an app of apps) and the Applications it made, or one
//! Application on its own; the type of each (prod, staging…) from its name; and the images
//! each runs.
//!
//! An environment's root is the highest Application above it that isn't a *bootstrap*: one
//! that made an app of apps (a `root` making every environment's root). So `root →
//! shop-prod-a → {shop-prod-a-workload, shop-prod-a-managed-namespace}` is the environment
//! `shop-prod-a` of three Applications, and `root` one of its own. Parents in a loop still put
//! every Application in exactly one environment.
//!
//! The type comes from the root's name, by patterns: one `type = regex` per line (Settings →
//! Argo CD), in the order shown. `type = tag:base` is an environment of type `base` whose
//! product images all carry a version tag (`acc on a tag = tag:acc`). The match that starts
//! first in the name wins, ties go to the earlier line: `platform-prod-x` is platform.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use regex::Regex;

use super::dashboard::split_image;

// MARK: Environments

/// The root of each Application's environment (see the module's doc), from the Application
/// that made each (`parents`, indexes; one out of range or naming itself is none).
pub fn roots(parents: &[Option<usize>]) -> Vec<usize> {
    let n = parents.len();
    let parent = |i: usize| parents[i].filter(|&p| p < n && p != i);
    let mut children = vec![Vec::new(); n];
    for i in 0..n {
        if let Some(p) = parent(i) {
            children[p].push(i);
        }
    }
    let bootstrap: Vec<bool> = children.iter().map(|c| c.iter().any(|&c| !children[c].is_empty())).collect();
    (0..n)
        .map(|i| {
            // Every Application in a loop of parents is a bootstrap (the next one in the loop
            // has a child), so the walk stops before going round; `n` steps bound it anyway.
            let mut at = i;
            for _ in 0..n {
                match parent(at) {
                    Some(p) if !bootstrap[p] => at = p,
                    _ => break,
                }
            }
            at
        })
        .collect()
}

/// The environments, from each Application's root (`roots`) and a tree order (parents before
/// their children, as `tree_order` gives): each environment's Applications in that order, its
/// root first, each with its depth under the root. In the order of their roots.
pub fn group(roots: &[usize], order: &[(usize, usize)]) -> Vec<Vec<(usize, usize)>> {
    let n = roots.len();
    let mut env_of: Vec<Option<usize>> = vec![None; n];
    let mut out: Vec<Vec<(usize, usize)>> = Vec::new();
    for &r in order.iter().map(|(i, _)| i).filter(|&&i| i < n && roots[i] == i) {
        env_of[r] = Some(out.len());
        out.push(Vec::new());
    }
    let mut depth_of = vec![0; n];
    for &(i, depth) in order.iter().filter(|(i, _)| *i < n) {
        depth_of[i] = depth;
    }
    for &(i, depth) in order.iter().filter(|(i, _)| *i < n) {
        let r = roots[i];
        let Some(env) = env_of.get(r).copied().flatten() else { continue };
        let under = if i == r { 0 } else { depth.saturating_sub(depth_of[r]).max(1) };
        if i == r {
            out[env].insert(0, (i, 0));
        } else {
            out[env].push((i, under));
        }
    }
    out
}

// MARK: Types

/// The environment types, in the order shown, with the rule of each.
pub const DEFAULT_TYPES: [&str; 7] = [
    "prod = -prod(-|$)",
    "preprod = -preprod(-|$)",
    "acc on a tag = tag:acc",
    "acc = -acc[0-9]*(-|$)",
    "staging = -staging(-|$)",
    "review = -review(-|$)",
    "platform = ^platform-",
];

/// The types of production environments, or close to them: a grouped sync that includes one
/// asks for a second click. A type of Settings named with `prod` is one too.
pub const PRODUCTION_TYPES: [&str; 4] = ["prod", "preprod", "acc on a tag", "platform"];

/// Whether environments of the type named `name` are production ones (see [`PRODUCTION_TYPES`]).
pub fn is_production(name: &str) -> bool {
    PRODUCTION_TYPES.contains(&name) || name.contains("prod")
}

/// The type of an environment no pattern matches.
pub const OTHER: &str = "other";

/// How a type is told.
#[derive(Clone, Debug)]
enum Rule {
    /// A pattern of the root's name.
    Name(Regex),
    /// An environment of the type at this index, its product images all on a version tag.
    Tagged(usize),
}

/// The environment types, read from their lines (see the module's doc).
#[derive(Clone, Debug)]
pub struct EnvTypes {
    names: Vec<String>,
    rules: Vec<Rule>,
    /// The lines that don't read, and why: they type nothing.
    pub errors: Vec<String>,
}

impl Default for EnvTypes {
    fn default() -> Self {
        Self::parse(&DEFAULT_TYPES.map(String::from))
    }
}

impl EnvTypes {
    /// The types of `lines` (blank lines and `#` comments skipped; none at all: the
    /// defaults). A line that doesn't read is said in `errors` and types nothing.
    pub fn parse(lines: &[String]) -> Self {
        enum Raw {
            Pattern(Regex),
            Tag(String),
        }
        let lines: Vec<&str> =
            lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
        if lines.is_empty() {
            return Self::default();
        }
        let mut errors = Vec::new();
        let mut read: Vec<(String, Raw)> = Vec::new();
        for line in lines {
            let Some((name, rule)) = line.split_once('=').map(|(n, r)| (n.trim(), r.trim())) else {
                errors.push(format!("`{line}`: not `type = pattern`"));
                continue;
            };
            if name.is_empty() || name == OTHER || read.iter().any(|(n, _)| n == name) {
                errors.push(format!("`{line}`: a type is named, once, and not `{OTHER}`"));
                continue;
            }
            let raw = match rule.strip_prefix("tag:") {
                Some(base) => Raw::Tag(base.trim().to_string()),
                None => match Regex::new(rule) {
                    Ok(re) => Raw::Pattern(re),
                    Err(e) => {
                        errors.push(format!("`{line}`: {}", e.to_string().lines().last().unwrap_or_default().trim()));
                        continue;
                    }
                },
            };
            read.push((name.to_string(), raw));
        }
        let patterns: HashSet<String> =
            read.iter().filter(|(_, r)| matches!(r, Raw::Pattern(_))).map(|(n, _)| n.clone()).collect();
        read.retain(|(name, raw)| match raw {
            Raw::Tag(base) if !patterns.contains(base) => {
                errors.push(format!("`{name} = tag:{base}`: no type `{base}` with a pattern"));
                false
            }
            _ => true,
        });
        let names: Vec<String> = read.iter().map(|(n, _)| n.clone()).collect();
        let rules = read
            .into_iter()
            .map(|(_, raw)| match raw {
                Raw::Pattern(re) => Rule::Name(re),
                Raw::Tag(base) => Rule::Tagged(names.iter().position(|n| *n == base).unwrap_or(usize::MAX)),
            })
            .collect();
        Self { names, rules, errors }
    }

    /// The name of type `t` (an index `of` gives): `other` past the types.
    pub fn name(&self, t: usize) -> &str {
        self.names.get(t).map_or(OTHER, String::as_str)
    }

    /// The client of an environment whose root is named `name`: what follows its type's match
    /// (`acme` in `shop-prod-acme`), when something does.
    pub fn client<'a>(&self, name: &'a str) -> Option<&'a str> {
        let m = self
            .rules
            .iter()
            .filter_map(|r| match r {
                Rule::Name(re) => re.find(name),
                Rule::Tagged(_) => None,
            })
            .min_by_key(|m| m.start())?;
        Some(name[m.end()..].trim_matches('-')).filter(|c| !c.is_empty())
    }

    /// The type of an environment whose root is named `name`, `on_tag` when its product
    /// images all carry a version tag: an index of the types, the last (`other`) when no
    /// pattern matches.
    pub fn of(&self, name: &str, on_tag: bool) -> usize {
        let mut best: Option<(usize, usize)> = None;
        for (t, rule) in self.rules.iter().enumerate() {
            if let Rule::Name(re) = rule
                && let Some(m) = re.find(name)
                && best.is_none_or(|(start, _)| m.start() < start)
            {
                best = Some((m.start(), t));
            }
        }
        let Some((_, t)) = best else { return self.names.len() };
        match self.rules.iter().position(|r| matches!(r, Rule::Tagged(b) if *b == t)) {
            Some(tagged) if on_tag => tagged,
            _ => t,
        }
    }
}

// MARK: Images

/// An image an environment runs: its repository, the last part of it (`web`), and its tags
/// (several when its Applications run it at several).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvImage {
    pub repo: String,
    pub component: String,
    pub tags: Vec<String>,
}

/// The images of an environment's Applications (`status.summary.images`), by repository,
/// sorted by component.
pub fn images<'a>(all: impl Iterator<Item = &'a str>) -> Vec<EnvImage> {
    let mut by_repo: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for image in all {
        let (repo, tag) = split_image(image);
        by_repo.entry(repo).or_default().insert(tag);
    }
    let mut out: Vec<EnvImage> = by_repo
        .into_iter()
        .map(|(repo, tags)| {
            let component = repo.rsplit('/').next().unwrap_or(&repo).to_string();
            EnvImage { repo, component, tags: tags.into_iter().collect() }
        })
        .collect();
    out.sort_by(|a, b| (&a.component, &a.repo).cmp(&(&b.component, &b.repo)));
    out
}

/// The repositories whose tags differ between the environments `envs` (one project's): the
/// product's own images, deployed at its own pace, not the third-party ones every environment
/// runs alike (a database, an exporter). One that only some environments run counts when
/// their tags differ.
pub fn differing(envs: &[&[EnvImage]]) -> HashSet<String> {
    let mut tags: BTreeMap<&str, BTreeSet<&[String]>> = BTreeMap::new();
    for env in envs {
        for i in *env {
            tags.entry(&i.repo).or_default().insert(&i.tags);
        }
    }
    tags.into_iter().filter(|(_, t)| t.len() > 1).map(|(r, _)| r.to_string()).collect()
}

/// An environment's images, the product's (`product`) first: (whether it is the product's,
/// the image).
pub fn product_first<'a>(images: &'a [EnvImage], product: &HashSet<String>) -> Vec<(bool, &'a EnvImage)> {
    let mut out: Vec<(bool, &EnvImage)> = images.iter().map(|i| (product.contains(&i.repo), i)).collect();
    out.sort_by_key(|(own, _)| !own);
    out
}

/// Whether an image repository is named after `name` (a project, an Application): a part of
/// its path past the registry is `name`, or starts with `name-` (`shop/api`, `team/shop-web`).
/// A third-party image (`library/redis`) isn't.
pub fn named_after(repo: &str, name: &str) -> bool {
    let name = name.trim().to_lowercase();
    if name.len() < 2 {
        return false;
    }
    let path = match repo.split_once('/') {
        Some((registry, path)) if registry.contains(['.', ':']) => path,
        _ => repo,
    };
    path.to_lowercase().split('/').any(|p| p == name || p.strip_prefix(&name).is_some_and(|r| r.starts_with('-')))
}

/// Whether a tag is a version (`1.4.2`, `v2.0`, `26-8-4-<sha>`), not a branch's build
/// (`main-<sha>`) or `latest`.
pub fn version_tag(tag: &str) -> bool {
    static VERSION: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^v?[0-9]+[.-][0-9]+([.-][0-9]+)?([.+-]|$)").expect("a valid pattern"));
    VERSION.is_match(tag)
}

/// Whether an environment runs its product on version tags: it runs some of the product's
/// images (`product`), all at a version tag. With no image of the product known, its images.
pub fn on_tag(images: &[EnvImage], product: &HashSet<String>) -> bool {
    let own: Vec<&EnvImage> = images.iter().filter(|i| product.contains(&i.repo)).collect();
    let own = if own.is_empty() { images.iter().collect() } else { own };
    !own.is_empty() && own.iter().all(|i| i.tags.iter().all(|t| version_tag(t)))
}

// MARK: Example

/// Made-up Applications for the developer tools' snapshot of the list
/// (`RECOUVRANCE_ACT=page:argo,argo-example-list`) and the tests: two products' environments
/// of every type, each a root and its `-managed-namespace` and `-workload`, and two platform
/// Applications. No real data.
#[cfg(any(test, feature = "dev"))]
pub fn example_apps(now: jiff::Timestamp) -> Vec<serde_json::Value> {
    use serde_json::json;
    let sha = |n: u8| format!("{n:x}7b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e").chars().take(40).collect::<String>();
    let ago = |mins: i64| jiff::Timestamp::from_second(now.as_second() - mins * 60).unwrap_or(now).to_string();
    // (project, environment, client, tag, what is wrong: "" | "oos" | "degraded" | "failed" | "running")
    let envs: Vec<(&str, &str, &str, String, &str)> = vec![
        ("shop", "prod", "acme", "1.4.2".into(), ""),
        ("shop", "prod", "globex", "1.4.1".into(), "oos"),
        ("shop", "preprod", "acme", "1.5.0-rc.1".into(), ""),
        ("shop", "acc2", "acme", "1.5.0-rc.1".into(), ""),
        ("shop", "acc", "globex", format!("main-{}", sha(1)), "running"),
        ("shop", "staging", "acme", format!("main-{}", sha(2)), "failed"),
        ("shop", "review", "jdoe-cart", format!("feat-cart-{}", sha(3)), ""),
        ("billing", "prod", "acme", "26-8-4-77b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e".into(), ""),
        ("billing", "preprod", "acme", "26-9-0-rc-1-77b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e".into(), "degraded"),
        ("billing", "staging", "acme", format!("main-{}", sha(4)), ""),
    ];
    let app =
        |name: &str, project: &str, ns: &str, parent: Option<&str>, images: Vec<String>, state: &str, mins: i64| {
            let (sync, health, phase) = match state {
                "oos" => ("OutOfSync", "Healthy", "Succeeded"),
                "degraded" => ("Synced", "Degraded", "Succeeded"),
                "failed" => ("OutOfSync", "Healthy", "Failed"),
                "running" => ("OutOfSync", "Progressing", "Running"),
                _ => ("Synced", "Healthy", "Succeeded"),
            };
            let mut v = json!({
                "metadata": {"name": name, "namespace": "argocd"},
                "spec": {"project": project, "destination": {"name": "ovh-prod-gra", "namespace": ns}},
                "status": {
                    "sync": {"status": sync, "revision": sha(9)},
                    "health": {"status": health},
                    "operationState": {"phase": phase, "startedAt": ago(mins + 1), "finishedAt": ago(mins)},
                    "summary": {"images": images},
                },
            });
            if let Some(p) = parent {
                v["metadata"]["annotations"] =
                    json!({super::TRACKING_ID: format!("{p}:argoproj.io/Application:argocd/{name}")});
            }
            if phase == "Running" {
                v["status"]["operationState"].as_object_mut().map(|o| o.remove("finishedAt"));
            }
            v
        };
    let mut out = Vec::new();
    for (i, (project, env, client, tag, wrong)) in envs.into_iter().enumerate() {
        let root = format!("{project}-{env}-{client}");
        let mins = 7 + 53 * i as i64;
        let images = vec![
            format!("registry.example.com/{project}/api:{tag}"),
            format!("registry.example.com/{project}/web:{tag}"),
            "docker.io/library/redis:7.2".to_string(),
            "quay.io/prometheus/statsd-exporter:v0.26.0".to_string(),
        ];
        out.push(app(&root, project, "argocd", None, Vec::new(), "", mins));
        out.push(app(&format!("{root}-managed-namespace"), project, &root, Some(&root), Vec::new(), "", mins));
        out.push(app(&format!("{root}-workload"), project, &root, Some(&root), images, wrong, mins));
    }
    for (name, image) in [
        ("platform-prod-monitoring", "quay.io/prometheus/prometheus:v3.1.0"),
        ("platform-staging-ingress", "registry.k8s.io/ingress-nginx/controller:v1.12.0"),
    ] {
        out.push(app(name, "platform", "platform", None, vec![image.to_string()], "", 600));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> EnvTypes {
        EnvTypes::default()
    }

    #[test]
    fn an_image_is_named_after_its_project_by_a_part_of_its_path() {
        assert!(named_after("registry.example.com/shop/api", "shop"));
        assert!(named_after("registry.example.com/team/shop-web", "shop"));
        assert!(named_after("registry.example.com/team/shop-web", "Shop"));
        assert!(!named_after("registry.example.com/team/shopping", "shop"));
        assert!(!named_after("docker.io/library/redis", "shop"));
        assert!(!named_after("shop.example.com/library/redis", "shop"), "the registry's name doesn't count");
        assert!(named_after("shop/api", "shop"), "no registry: the whole path");
        assert!(!named_after("registry.example.com/x/y", ""));
    }

    #[test]
    fn an_environment_is_its_root_and_what_the_root_made() {
        // 0 bootstrap: made 1 and 4 (each an app of apps); 1 made 2 and 3; 4 made 5; 6 alone.
        let parents = [None, Some(0), Some(1), Some(1), Some(0), Some(4), None];
        let r = roots(&parents);
        assert_eq!(r, [0, 1, 1, 1, 4, 4, 6]);
        let order = [(0, 0), (1, 1), (2, 2), (3, 2), (4, 1), (5, 2), (6, 0)];
        assert_eq!(group(&r, &order), [vec![(0, 0)], vec![(1, 0), (2, 1), (3, 1)], vec![(4, 0), (5, 1)], vec![(6, 0)]]);
        // Without a bootstrap above, the app of apps is the top.
        assert_eq!(roots(&[None, Some(0), Some(0)]), [0, 0, 0]);
    }

    /// The example's environments: each root with its two children, typed by name and tag.
    #[test]
    fn the_example_is_environments_of_three() {
        let apps = example_apps(jiff::Timestamp::UNIX_EPOCH);
        let parents = super::super::parents(&apps);
        let r = roots(&parents);
        let envs: BTreeSet<usize> = r.iter().copied().collect();
        assert_eq!(envs.len(), 12);
        let name = |i: usize| apps[i]["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let t = types();
        let tagged = |root: &str| !root.contains("acc-globex");
        let kinds: Vec<String> =
            envs.iter().map(|&e| format!("{}: {}", name(e), t.name(t.of(&name(e), tagged(&name(e)))))).collect();
        assert!(kinds.contains(&"shop-acc2-acme: acc on a tag".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"shop-acc-globex: acc".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"platform-prod-monitoring: platform".to_string()), "{kinds:?}");
    }

    #[test]
    fn a_type_is_the_first_pattern_in_the_name() {
        let t = types();
        let name = |root: &str, on_tag: bool| t.name(t.of(root, on_tag)).to_string();
        assert_eq!(name("cayzn-prod-acme", true), "prod");
        assert_eq!(name("cayzn-preprod-acme", false), "preprod");
        assert_eq!(name("cayzn-acc2-acme", false), "acc");
        assert_eq!(name("cayzn-acc2-acme", true), "acc on a tag");
        assert_eq!(name("cayzn-staging", false), "staging");
        assert_eq!(name("cayzn-review-jdoe-feat", false), "review");
        // `platform-` starts the name: before `-prod`.
        assert_eq!(name("platform-prod-monitoring", false), "platform");
        assert_eq!(name("production-line", false), OTHER);
        assert_eq!(t.name(t.of("x", false)), OTHER);
    }

    #[test]
    fn the_client_follows_the_type_in_the_name() {
        let t = types();
        assert_eq!(t.client("shop-prod-acme"), Some("acme"));
        assert_eq!(t.client("shop-acc2-globex-eu"), Some("globex-eu"));
        assert_eq!(t.client("shop-staging"), None);
        assert_eq!(t.client("production-line"), None);
    }

    #[test]
    fn a_line_that_does_not_read_types_nothing() {
        let lines = |l: &[&str]| EnvTypes::parse(&l.iter().map(ToString::to_string).collect::<Vec<_>>());
        let t = lines(&["live = -live$", "broken = (", "no pattern", "onTag = tag:missing", "other = x", "live = y"]);
        assert_eq!(t.errors.len(), 5, "{:?}", t.errors);
        assert_eq!((t.name(0), t.name(1)), ("live", OTHER));
        assert_eq!(t.name(t.of("shop-live", true)), "live");
        assert_eq!(t.name(t.of("shop-prod", true)), OTHER);
        // No line: the defaults.
        let defaults = lines(&["", "# nothing"]);
        assert_eq!((defaults.name(DEFAULT_TYPES.len() - 1), defaults.name(DEFAULT_TYPES.len())), ("platform", OTHER));
    }

    #[test]
    fn production_types() {
        for t in ["prod", "preprod", "acc on a tag", "platform", "prod-eu"] {
            assert!(is_production(t), "{t}");
        }
        for t in ["acc", "staging", "review", OTHER] {
            assert!(!is_production(t), "{t}");
        }
    }

    #[test]
    fn the_product_s_images_are_those_that_differ() {
        let env = |api: &str| {
            images(
                [format!("r.example/shop/api:{api}"), "redis:7.2".to_string(), "r.example/shop/web:1.0.0".to_string()]
                    .iter()
                    .map(String::as_str),
            )
        };
        let (a, b) = (env("1.4.0"), env("main-77b324b"));
        let product = differing(&[&a, &b]);
        assert_eq!(product, HashSet::from(["r.example/shop/api".to_string()]));
        assert_eq!(product_first(&a, &product)[0].1.component, "api");
        assert!(on_tag(&a, &product) && !on_tag(&b, &product));
        // One environment alone: all its images say.
        assert!(on_tag(&env("2.0.0")[1..], &HashSet::new()));
        for v in ["1.4.2", "v2.0", "26-8-4-77b324b4", "1.0.0-rc.1"] {
            assert!(version_tag(v), "{v}");
        }
        for b in ["main-77b324b4", "latest", "feat-1-2", "@sha256abc"] {
            assert!(!version_tag(b), "{b}");
        }
    }
}

#[cfg(test)]
mod properties {
    use super::{group, roots};
    use proptest::prelude::*;

    proptest! {
        /// Every Application is in exactly one environment, its root's, loops of parents
        /// included; a root is its own; one made by an Application that is no bootstrap is in
        /// its parent's.
        #[test]
        fn every_application_is_in_one_environment(parents in proptest::collection::vec(proptest::option::of(0usize..14), 0..14)) {
            let r = roots(&parents);
            for (i, &root) in r.iter().enumerate() {
                prop_assert_eq!(r[root], root);
                prop_assert!(root < parents.len());
                let _ = i;
            }
            let order = crate::ui::argo::tree_order(&parents, |a, b| a.cmp(&b));
            let envs = group(&r, &order);
            let mut seen: Vec<usize> = envs.iter().flatten().map(|(i, _)| *i).collect();
            seen.sort_unstable();
            prop_assert_eq!(seen, (0..parents.len()).collect::<Vec<_>>());
            for env in &envs {
                let root = env[0].0;
                prop_assert_eq!(r[root], root);
                prop_assert!(env.iter().all(|(i, _)| r[*i] == root));
            }
        }
    }
}
