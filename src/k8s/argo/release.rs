//! A product's releases, as the Versions card shows them: the images of its running workloads
//! (not those of finished Jobs), one chip per component with its release version, and the
//! previous release (the last deployment whose product versions differ).
//!
//! - **Which images.** Those of the workloads' pod templates (Deployments, StatefulSets,
//!   DaemonSets, ReplicaSets, CronJobs, Argo Rollouts), live or as git renders them. The
//!   product's are those Settings → Argo CD names (`image pattern = short name`), else the
//!   page's guess.
//! - **One chip per component.** A repository's tags are one chip; variants of a repository
//!   (`app`, `app-chrome`) built from the same commit (their tags hold the same sha) are too;
//!   Settings can join others under one name. Its short name drops what it shares with its project (`shop/shop-api` is `api`).
//! - **A tag's release version** ([`version_of`]): a date-or-semver-looking prefix is the
//!   version (`v26-11-31-<sha>-chrome` is `v26.11.31`), compared as numbers; a branch's build
//!   is the branch and the commit's first 8 characters (`main-<sha>` is `main-1a2b3c4d`).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde_json::Value;

use super::dashboard::split_image;

// MARK: Versions

/// A tag's release version: its text, and its numbers when it is one (`v26.11.31`: 26, 11, 31).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub text: String,
    /// Empty when the tag isn't a version (a branch's build, `latest`).
    numbers: Vec<u64>,
    /// Written with a `v` before it.
    v: bool,
    /// A pre-release (`-rc.1`): its numbers, after the version's.
    pre: Option<Vec<u64>>,
}

impl Version {
    /// Whether it is a version, compared as numbers.
    pub fn numeric(&self) -> bool {
        !self.numbers.is_empty()
    }

    /// How `self` compares with `other` as numbers (`v26.11.9` < `v26.11.32`), when both are
    /// versions written alike (as many numbers, both with a `v` or without). A pre-release
    /// comes before its version.
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        if !self.numeric() || self.numbers.len() != other.numbers.len() || self.v != other.v {
            return None;
        }
        Some(self.numbers.cmp(&other.numbers).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }))
    }
}

/// The digits at the start of `s`, and what follows them.
fn digits(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(end)
}

/// Numbers separated by `.`, `-` or `_` at the start of `s` (each followed by a separator or the
/// end), and what follows the last. A run of 7 digits or more after the first number is a
/// commit's sha, not a number of the version.
fn numbers(s: &str) -> (Vec<u64>, &str) {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        let (d, after) = digits(rest);
        let ends = after.is_empty() || after.starts_with(['.', '-', '_', '+']);
        if d.is_empty() || !ends || (!out.is_empty() && d.len() >= 7) {
            break;
        }
        let Ok(n) = d.parse::<u64>() else { break };
        out.push(n);
        rest = after;
        match rest.strip_prefix(['.', '-', '_']) {
            Some(next) if next.starts_with(|c: char| c.is_ascii_digit()) => rest = next,
            _ => break,
        }
    }
    (out, rest)
}

/// Whether a part of a tag is a commit's sha: 7 to 40 hex digits, with a letter in them (a
/// build number isn't one).
fn is_sha(part: &str) -> bool {
    (7..=40).contains(&part.len())
        && part.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && part.bytes().any(|b| b.is_ascii_alphabetic())
}

/// The commit's sha a tag holds, when a part of it (between `-`) is one.
pub fn sha_in(tag: &str) -> Option<&str> {
    tag.split('-').skip(1).find(|p| is_sha(p))
}

/// The release version of an image's tag (see the module's doc): a version-looking prefix
/// (`v26-11-31-<sha>-chrome` → `v26.11.31`, `1.5.0-rc.1` → `1.5.0-rc.1`, `2.3.1-alpine` →
/// `2.3.1`), else a branch's build (`main-<sha>[-variant]` → `main-<8 characters>`), else the
/// tag as it is.
pub fn version_of(tag: &str) -> Version {
    let (v, body) = match tag.strip_prefix(['v', 'V']) {
        Some(b) if b.starts_with(|c: char| c.is_ascii_digit()) => (true, b),
        _ => (false, tag),
    };
    let (nums, rest) = numbers(body);
    if !nums.is_empty() {
        let mut text =
            format!("{}{}", if v { "v" } else { "" }, nums.iter().map(u64::to_string).collect::<Vec<_>>().join("."));
        // A pre-release marker right after: `-rc.1`, `-beta-2`.
        let mut pre = None;
        let marker = rest.strip_prefix(['-', '.', '_']).unwrap_or("");
        let lower = marker.to_ascii_lowercase();
        if let Some(word) = ["rc", "alpha", "beta", "pre"].into_iter().find(|w| lower.starts_with(w)) {
            let after = &marker[word.len()..];
            let after = after.strip_prefix(['.', '-', '_']).unwrap_or(after);
            let (p, _) = numbers(after);
            let shown = if p.is_empty() {
                word.to_string()
            } else {
                format!("{word}.{}", p.iter().map(u64::to_string).collect::<Vec<_>>().join("."))
            };
            text.push_str(&format!("-{shown}"));
            pre = Some(p);
        }
        return Version { text, numbers: nums, v, pre };
    }
    let parts: Vec<&str> = tag.split('-').collect();
    if let Some(i) = parts.iter().skip(1).position(|p| is_sha(p)).map(|i| i + 1) {
        let sha: String = parts[i].chars().take(8).collect();
        return Version { text: format!("{}-{sha}", parts[..i].join("-")), ..Version::default() };
    }
    if is_sha(tag) {
        return Version { text: tag.chars().take(8).collect(), ..Version::default() };
    }
    Version { text: tag.to_string(), ..Version::default() }
}

/// The newest of `tags` that is newer than `deployed`, compared as numbers among those written
/// alike: a release, so no pre-release unless `deployed` is one. None when `deployed` isn't a
/// version, or nothing is newer.
pub fn newest<'a>(tags: impl IntoIterator<Item = &'a str>, deployed: &Version) -> Option<(String, Version)> {
    if !deployed.numeric() {
        return None;
    }
    let mut best: Option<(String, Version)> = None;
    for tag in tags {
        let v = version_of(tag);
        if v.pre.is_some() && deployed.pre.is_none() {
            continue;
        }
        if v.compare(deployed) != Some(Ordering::Greater) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| v.compare(b) == Some(Ordering::Greater)) {
            best = Some((tag.to_string(), v));
        }
    }
    best
}

// MARK: Images

/// Where a workload kind keeps its pod's spec.
fn pod_spec(o: &Value) -> Option<&Value> {
    let kind = o.get("kind").and_then(Value::as_str)?;
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Rollout" => o.pointer("/spec/template/spec"),
        "CronJob" => o.pointer("/spec/jobTemplate/spec/template/spec"),
        _ => None,
    }
}

/// The images of the workloads among `objects` (their pod templates' containers and init
/// containers), each once, in order. A Job or a Pod (a finished run, a hook) isn't one.
pub fn workload_images<'a>(objects: impl IntoIterator<Item = &'a Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for spec in objects.into_iter().filter_map(pod_spec) {
        for key in ["initContainers", "containers"] {
            let list = spec.get(key).and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
            for image in list.iter().filter_map(|c| c.get("image")?.as_str()) {
                if !out.iter().any(|i| i == image) {
                    out.push(image.to_string());
                }
            }
        }
    }
    out
}

/// The images of the workloads Argo CD manages (its managed resources' answer): as they run
/// (`liveState`), and as git renders them (`targetState`), hooks left out.
pub fn managed_images(body: &Value) -> (Vec<String>, Vec<String>) {
    let items = body.get("items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let state = |item: &Value, key: &str| -> Option<Value> {
        serde_json::from_str(item.get(key)?.as_str()?).ok().filter(|v: &Value| v.is_object())
    };
    let workloads = items.iter().filter(|i| {
        i.get("hook").and_then(Value::as_bool) != Some(true)
            && matches!(
                i.get("kind").and_then(Value::as_str),
                Some("Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Rollout" | "CronJob")
            )
    });
    let (mut live, mut target) = (Vec::new(), Vec::new());
    for item in workloads {
        live.extend(state(item, "liveState"));
        target.extend(state(item, "targetState"));
    }
    (workload_images(&live), workload_images(&target))
}

// MARK: Names

/// One line of Settings → Argo CD's image names: `pattern = short name`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    pattern: String,
    name: String,
}

/// The short names given to the product's images (Settings → Argo CD): one `pattern = name` per
/// line. A pattern with `*` matches the whole repository (`*/shop-api*`); one without matches
/// the repository or its last part. Several patterns may give one name: one chip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    rules: Vec<Rule>,
    /// The lines that don't read, and why.
    pub errors: Vec<String>,
}

impl Names {
    /// The names of `lines` (blank lines and `#` comments skipped).
    pub fn parse(lines: &[String]) -> Self {
        let mut out = Self::default();
        for line in lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if let Some((pattern, name)) = line.split_once('=').map(|(p, n)| (p.trim(), n.trim()))
                && !pattern.is_empty()
                && !name.is_empty()
            {
                out.rules.push(Rule { pattern: pattern.to_lowercase(), name: name.to_string() });
            } else {
                out.errors.push(format!("`{line}`: not `image pattern = short name`"));
            }
        }
        out
    }

    /// The short name of the repository `repo`, when a line names it (the first that does).
    pub fn name_of(&self, repo: &str) -> Option<&str> {
        let repo = repo.to_lowercase();
        let last = repo.rsplit('/').next().unwrap_or(&repo);
        self.rules
            .iter()
            .find(|r| {
                if r.pattern.contains('*') {
                    crate::glob::glob(&r.pattern, &repo)
                } else {
                    r.pattern == repo || r.pattern == last
                }
            })
            .map(|r| r.name.as_str())
    }

    /// The position of the line that gives `name`: chips named by Settings come in its order.
    fn order(&self, name: &str) -> usize {
        self.rules.iter().position(|r| r.name == name).unwrap_or(usize::MAX)
    }
}

/// Whether environments of the type `kind` check the registry for a newer tag: the types
/// `saved` in Settings (`, `-separated lines), else the production ones.
pub fn checks_tags(saved: &[String], kind: &str) -> bool {
    let types: Vec<&str> = saved.iter().flat_map(|l| l.split(',')).map(str::trim).filter(|t| !t.is_empty()).collect();
    if types.is_empty() { super::environments::is_production(kind) } else { types.contains(&kind) }
}

// MARK: Chips

/// One component of the product, as a chip: its short name, its repositories (a variant's
/// too), its whole references (`repo:tag`), and its release version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub name: String,
    pub repos: Vec<String>,
    pub refs: Vec<String>,
    /// The newest version it runs (by number, else the first).
    pub version: Version,
    /// It runs at several versions at once.
    pub several: bool,
    /// The repository a newer tag is looked for in: its first (the shortest, not a variant).
    pub repo: String,
}

/// The release version of the images `tags`: the newest by number, else the first.
fn best(tags: &[&str]) -> (Version, bool) {
    let mut versions: Vec<Version> = tags.iter().map(|t| version_of(t)).collect();
    versions.dedup_by(|a, b| a.text == b.text);
    let several = versions.iter().map(|v| &v.text).collect::<std::collections::BTreeSet<_>>().len() > 1;
    let mut pick = versions.first().cloned().unwrap_or_default();
    for v in &versions {
        if v.compare(&pick) == Some(Ordering::Greater) {
            pick = v.clone();
        }
    }
    (pick, several)
}

/// The short name of a repository: its last part, without its parent's name before it
/// (`shop/shop-api` → `api`).
fn short(repo: &str) -> String {
    let mut parts = repo.rsplit('/');
    let last = parts.next().unwrap_or(repo);
    let parent = parts.next().unwrap_or_default();
    match last.strip_prefix(parent).and_then(|r| r.strip_prefix('-')) {
        Some(rest) if !parent.is_empty() && !rest.is_empty() => rest.to_string(),
        _ => last.to_string(),
    }
}

/// The product's chips among `images` (`repo:tag` references): those Settings names, else those
/// `guess` says are the product's (see the module's doc).
pub fn chips(images: &[String], names: &Names, guess: impl Fn(&str) -> bool) -> Vec<Chip> {
    let split: Vec<(String, String)> = images.iter().map(|i| split_image(i)).collect();
    let named = split.iter().any(|(r, _)| names.name_of(r).is_some());
    // By repository: its tags.
    let mut repos: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (repo, tag) in &split {
        let own = if named { names.name_of(repo).is_some() } else { guess(repo) };
        if own {
            let tags = repos.entry(repo.clone()).or_default();
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    // Grouped: by the name Settings gives, else a repository with its variants.
    let mut groups: Vec<(Option<String>, Vec<String>)> = Vec::new();
    let mut by_len: Vec<&String> = repos.keys().collect();
    by_len.sort_by_key(|r| (r.len(), (*r).clone()));
    for repo in by_len {
        if let Some(name) = names.name_of(repo).filter(|_| named) {
            match groups.iter_mut().find(|(n, _)| n.as_deref() == Some(name)) {
                Some((_, rs)) => rs.push(repo.clone()),
                None => groups.push((Some(name.to_string()), vec![repo.clone()])),
            }
            continue;
        }
        let variant_of = groups.iter_mut().find(|(n, rs)| {
            n.is_none() && repo.strip_prefix(rs[0].as_str()).is_some_and(|r| r.starts_with('-')) && {
                // Built from the same commit: one tag of each holds the same sha.
                let same = |a: &[String], b: &[String]| {
                    let shas = |t: &[String]| t.iter().filter_map(|t| sha_in(t).map(String::from)).collect::<Vec<_>>();
                    let sb = shas(b);
                    shas(a).iter().any(|s| sb.contains(s))
                };
                same(&repos[&rs[0]], &repos[repo])
            }
        });
        match variant_of {
            Some((_, rs)) => rs.push(repo.clone()),
            None => groups.push((None, vec![repo.clone()])),
        }
    }
    let mut out: Vec<(usize, Chip)> = groups
        .into_iter()
        .map(|(name, rs)| {
            let mut refs: Vec<String> = Vec::new();
            let mut tags: Vec<&str> = Vec::new();
            for r in &rs {
                for t in &repos[r] {
                    refs.push(format!("{r}:{t}"));
                    tags.push(t);
                }
            }
            let (version, several) = best(&tags);
            let order = name.as_deref().map_or(usize::MAX, |n| names.order(n));
            let name = name.unwrap_or_else(|| short(&rs[0]));
            (order, Chip { name, repo: rs[0].clone(), repos: rs, refs, version, several })
        })
        .collect();
    out.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)));
    out.into_iter().map(|(_, c)| c).collect()
}

// MARK: Releases

/// What a deployment ran of each chip (in the chips' order): its version, or None when it ran
/// none of its repositories.
pub type Release = Vec<Option<String>>;

/// The release `images` make of `chips`: each chip's version among them.
pub fn release_in(chips: &[Chip], images: &[String]) -> Release {
    let split: Vec<(String, String)> = images.iter().map(|i| split_image(i)).collect();
    chips
        .iter()
        .map(|c| {
            let tags: Vec<&str> = split.iter().filter(|(r, _)| c.repos.contains(r)).map(|(_, t)| t.as_str()).collect();
            (!tags.is_empty()).then(|| best(&tags).0.text)
        })
        .collect()
}

/// The images a deployment's sources pin, from their values tree (`sync_diff::source_tree`:
/// one source as is, several as `source 1`, `source 2`…): kustomize's images and the Helm
/// parameters `<x>repository` with their `<x>tag`.
pub fn pinned_images(tree: &Value) -> Vec<String> {
    let Some(o) = tree.as_object() else { return Vec::new() };
    let sources: Vec<&Value> =
        if o.keys().any(|k| k.starts_with("source ")) { o.values().collect() } else { vec![tree] };
    let mut out = Vec::new();
    for s in sources {
        out.extend(super::dashboard::source_images(s));
        if let Some(images) = s.pointer("/kustomize/images").and_then(Value::as_object) {
            out.extend(images.values().filter_map(Value::as_str).map(String::from));
        }
    }
    out
}

/// The release that images pinned in a deployment's sources make (kustomize images, Helm
/// parameters), when they pin every chip; None when some chip isn't pinned (its release then
/// has to be rendered).
pub fn pinned_release(chips: &[Chip], pinned: &[String]) -> Option<Release> {
    let r = release_in(chips, pinned);
    (!chips.is_empty() && r.iter().all(Option::is_some)).then_some(r)
}

/// The previous release among the deployments `before` the newest (oldest first; each one's
/// release, None while not known): the last whose release differs from `current`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The deployment at this index.
    Found(usize),
    /// The deployment at this index has to be known first: every one after it has `current`.
    Needs(usize),
    /// Every deployment before ran `current`.
    None,
}

pub fn previous_release(current: &Release, before: &[Option<Release>]) -> Pick {
    for (i, r) in before.iter().enumerate().rev() {
        match r {
            None => return Pick::Needs(i),
            Some(r) if r != current => return Pick::Found(i),
            Some(_) => {}
        }
    }
    Pick::None
}

/// One chip's version before and after a sync or a rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl Change {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }

    /// `app v1 → v2`, or `app v1 unchanged`.
    #[cfg(test)]
    pub fn text(&self) -> String {
        let side = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".into());
        if self.changed() {
            format!("{} {} → {}", self.name, side(&self.before), side(&self.after))
        } else {
            format!("{} {} unchanged", self.name, side(&self.before))
        }
    }
}

/// Each chip's version in `before` and `after`.
pub fn changes(chips: &[Chip], before: &Release, after: &Release) -> Vec<Change> {
    chips
        .iter()
        .enumerate()
        .map(|(i, c)| Change {
            name: c.name.clone(),
            before: before.get(i).cloned().flatten(),
            after: after.get(i).cloned().flatten(),
        })
        .collect()
}

/// The chips a previous release ran at other versions, said: `app v1.2.0, admin v3.1`.
pub fn previous_text(changes: &[Change]) -> String {
    changes
        .iter()
        .filter(|c| c.changed())
        .map(|c| format!("{} {}", c.name, c.after.as_deref().unwrap_or("none")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SHA: &str = "f98b1b4e22eeb45ea159a40e4259f72d120f5ca0";

    #[test]
    fn a_tag_reads_as_its_release_version() {
        let v = |t: &str| version_of(t).text;
        assert_eq!(v(&format!("v26-11-31-{SHA}")), "v26.11.31");
        assert_eq!(v(&format!("v26-11-31-{SHA}-chrome")), "v26.11.31");
        assert_eq!(v("v26-11-31-f98b1b4-whitelist-proxy"), "v26.11.31");
        assert_eq!(v("v26-11-3-7c40d67"), "v26.11.3", "a sha starting with a digit isn't a number");
        assert_eq!(v(&format!("master-{SHA}")), "master-f98b1b4e");
        assert_eq!(v(&format!("main-{SHA}-chrome")), "main-f98b1b4e");
        assert_eq!(v("1.4.2"), "1.4.2");
        assert_eq!(v("2.3.1-alpine"), "2.3.1");
        assert_eq!(v("1.5.0-rc.1"), "1.5.0-rc.1");
        assert_eq!(v(&format!("26-9-0-rc-1-{SHA}")), "26.9.0-rc.1");
        assert_eq!(v("26-8-4-1234567"), "26.8.4", "seven digits after the first number: a build, not the version");
        assert_eq!(v("latest"), "latest");
        assert_eq!(v(SHA), "f98b1b4e");
        assert_eq!(v("@0123456789ab"), "@0123456789ab");
        assert_eq!(v("feat-cart-1a2b3c4"), "feat-cart-1a2b3c4");
        assert!(!version_of("main-1a2b3c4").numeric());
    }

    #[test]
    fn versions_compare_as_numbers_never_as_text() {
        let c = |a: &str, b: &str| version_of(a).compare(&version_of(b));
        assert_eq!(c("v26.11.9", "v26.11.32"), Some(Ordering::Less));
        assert_eq!(c("v26-11-32-abcdef1", "v26.11.9"), Some(Ordering::Greater));
        assert_eq!(c("v26.7.35", "v26.12.3"), Some(Ordering::Less));
        assert_eq!(c("1.10.0", "1.9.9"), Some(Ordering::Greater));
        assert_eq!(c("1.5.0-rc.1", "1.5.0"), Some(Ordering::Less));
        assert_eq!(c("1.5.0-rc.2", "1.5.0-rc.10"), Some(Ordering::Less));
        assert_eq!(c("v1.2", "1.2"), None, "written otherwise");
        assert_eq!(c("1.2", "1.2.3"), None);
        assert_eq!(c("main-abcdef1", "main-abcdef2"), None);
    }

    #[test]
    fn the_newest_release_tag_is_found_among_those_written_alike() {
        let deployed = version_of(&format!("v26-11-22-{SHA}"));
        let tags = [
            "v26-11-9-aaaaaaa",
            "v26-11-23-bbbbbbb",
            "v26-11-23-bbbbbbb-chrome",
            "v26-11-3-ccccccc",
            "26-12-1-ddddddd",
            "latest",
            "master-eeeeeee",
            "v26-12-0-rc-1-fffffff",
        ];
        let (tag, v) = newest(tags, &deployed).unwrap();
        assert_eq!((tag.as_str(), v.text.as_str()), ("v26-11-23-bbbbbbb", "v26.11.23"));
        assert_eq!(newest(["v26-11-21-aaaaaaa", "v26-11-22-bbbbbbb"], &deployed), None, "nothing newer");
        assert_eq!(newest(["v9-9-9-aaaaaaa"], &version_of("master-abcdef1")), None, "not a version");
    }

    #[test]
    fn only_running_workloads_count() {
        let pod = |image: &str| json!({"containers": [{"name": "a", "image": image}]});
        let objects = [
            json!({"kind": "Deployment", "spec": {"template": {"spec": {
                "initContainers": [{"name": "i", "image": "r/app:1"}], "containers": [{"name": "a", "image": "r/app:1"}, {"name": "s", "image": "r/exporter:2"}]}}}}),
            json!({"kind": "CronJob", "spec": {"jobTemplate": {"spec": {"template": {"spec": pod("r/cron:3")}}}}}),
            json!({"kind": "Job", "spec": {"template": {"spec": pod("r/app:0")}}}),
            json!({"kind": "Pod", "spec": pod("r/app:old")}),
        ];
        assert_eq!(workload_images(&objects), ["r/app:1", "r/exporter:2", "r/cron:3"]);
        let state = |image: &str| json!({"kind": "Deployment", "spec": {"template": {"spec": pod(image)}}}).to_string();
        let body = json!({"items": [
            {"kind": "Deployment", "liveState": state("r/app:1"), "targetState": state("r/app:2")},
            {"kind": "Job", "hook": true, "liveState": state("r/migrate:1"), "targetState": state("r/migrate:2")},
            {"kind": "ConfigMap", "liveState": "{}", "targetState": "null"},
        ]});
        assert_eq!(managed_images(&body), (vec!["r/app:1".to_string()], vec!["r/app:2".to_string()]));
    }

    fn refs(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_component_and_its_variants_are_one_chip_with_a_short_name() {
        let images = refs(&[
            &format!("registry.example.com/shop/shop-app:v26-11-31-{SHA}"),
            &format!("registry.example.com/shop/shop-app-chrome:v26-11-31-{SHA}-chrome"),
            &format!("registry.example.com/shop/shop-app:v26-11-31-{SHA}-proxy"),
            "registry.example.com/shop/shop-app-admin:v26-11-8-7e7d7bd",
            "registry.example.com/shop/shop-front:v26-11-3-7c40d67",
            "docker.io/library/redis:7.2",
        ]);
        let chips = chips(&images, &Names::default(), |r| r.contains("/shop/"));
        let shown: Vec<(&str, &str)> = chips.iter().map(|c| (c.name.as_str(), c.version.text.as_str())).collect();
        assert_eq!(shown, [("app", "v26.11.31"), ("app-admin", "v26.11.8"), ("front", "v26.11.3")]);
        assert_eq!(chips[0].repos.len(), 2, "the variant built from the same commit joins it");
        assert_eq!(chips[0].refs.len(), 3);
        assert!(!chips[0].several);
    }

    #[test]
    fn settings_names_pick_and_name_the_chips() {
        let names = Names::parse(&refs(&["# the back", "*/shop-app* = back", "shop-front = front", "not a line"]));
        assert_eq!(names.errors.len(), 1);
        let images = refs(&[
            "registry.example.com/shop/shop-front:2.0.1",
            "registry.example.com/shop/shop-app:1.4.2",
            "registry.example.com/shop/shop-app-chrome:1.4.2",
            "registry.example.com/shop/exporter:0.7.0",
        ]);
        let chips = chips(&images, &names, |_| true);
        let shown: Vec<(&str, &str)> = chips.iter().map(|c| (c.name.as_str(), c.version.text.as_str())).collect();
        assert_eq!(shown, [("back", "1.4.2"), ("front", "2.0.1")], "in Settings' order, the guess unused");
        // No line matches: the guess applies.
        let none = Names::parse(&refs(&["other = x"]));
        assert_eq!(super::chips(&images, &none, |r| r.ends_with("exporter")).len(), 1);
    }

    #[test]
    fn production_types_check_tags_unless_settings_say_which() {
        assert!(checks_tags(&[], "prod"));
        assert!(checks_tags(&[], "preprod"));
        assert!(!checks_tags(&[], "staging"));
        let saved = refs(&["staging, review"]);
        assert!(checks_tags(&saved, "staging") && checks_tags(&saved, "review"));
        assert!(!checks_tags(&saved, "prod"));
        assert!(!checks_tags(&refs(&["none"]), "prod"), "a type that doesn't exist turns it off");
    }

    #[test]
    fn the_previous_release_is_the_last_deployment_with_other_versions() {
        let r = |v: &[&str]| -> Release { v.iter().map(|s| Some(s.to_string())).collect() };
        let current = r(&["v2", "v5"]);
        // #1 v1, #2 v2 (released), #3 v2 (a sync of other changes), newest left out.
        let before = [Some(r(&["v1", "v5"])), Some(r(&["v2", "v5"])), Some(r(&["v2", "v5"]))];
        assert_eq!(previous_release(&current, &before), Pick::Found(0));
        assert_eq!(previous_release(&current, &[None, Some(current.clone())]), Pick::Needs(0));
        assert_eq!(previous_release(&current, &[Some(r(&["v1", "v5"])), None]), Pick::Needs(1));
        assert_eq!(previous_release(&current, &[Some(current.clone())]), Pick::None);
        assert_eq!(previous_release(&current, &[]), Pick::None);
        // A chip the deployment didn't run makes it another release.
        assert_eq!(previous_release(&current, &[Some(vec![Some("v2".into()), None])]), Pick::Found(0));
    }

    #[test]
    fn releases_and_their_changes_read_from_images() {
        let images = refs(&["r.example.com/shop/shop-app:1.4.2", "r.example.com/shop/shop-admin:3.1.0"]);
        let chips = chips(&images, &Names::default(), |_| true);
        let now = release_in(&chips, &images);
        let before =
            release_in(&chips, &refs(&["r.example.com/shop/shop-app:1.4.1", "r.example.com/shop/shop-admin:3.1.0"]));
        let c = changes(&chips, &now, &before);
        assert_eq!(c.iter().map(Change::text).collect::<Vec<_>>(), ["admin 3.1.0 unchanged", "app 1.4.2 → 1.4.1"]);
        assert_eq!(previous_text(&c), "app 1.4.1");
        assert_eq!(pinned_release(&chips, &refs(&["r.example.com/shop/shop-app:1.4.1"])), None, "admin isn't pinned");
        assert!(pinned_release(&chips, &images).is_some());
    }

    #[test]
    fn images_pinned_in_sources_are_read_from_their_tree() {
        let one = json!({"source": {"repoURL": "https://git.example.com/deploy.git",
            "kustomize": {"images": ["r.example.com/shop/api:1.2", "web=r.example.com/shop/web:2.0"]}}});
        let mut pinned = pinned_images(&crate::k8s::argo::sync_diff::source_tree(&one));
        pinned.sort();
        assert_eq!(pinned, ["r.example.com/shop/api:1.2", "r.example.com/shop/web:2.0"]);
        let many = json!({"sources": [
            {"repoURL": "https://charts.example.com", "chart": "app", "helm": {"parameters": [
                {"name": "image.repository", "value": "r.example.com/shop/api"}, {"name": "image.tag", "value": "1.3"}]}},
            {"repoURL": "https://git.example.com/values.git"},
        ]});
        assert_eq!(pinned_images(&crate::k8s::argo::sync_diff::source_tree(&many)), ["r.example.com/shop/api:1.3"]);
        assert!(pinned_images(&json!(null)).is_empty());
    }
}
