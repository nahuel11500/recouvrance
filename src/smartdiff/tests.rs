use proptest::prelude::*;
use serde_json::{Value, json};

use super::rules::{self, kustomize_stem};
use super::{
    Edit, Fact, ObjectKey, ResourceChange, Seg, compare, compare_values, group, leaves, objects, parse_image, path_text,
};
use crate::history::DiffLine;

fn deployment(name: &str, tag: &str, replicas: i64, env: Value) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "shop", "labels": {"app": name}},
        "spec": {
            "replicas": replicas,
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"containers": [{"name": "app", "image": format!("registry.example.com/shop/web:{tag}"), "env": env}]}
            }
        }
    })
}

fn texts(d: &super::SmartDiff) -> Vec<String> {
    d.groups.iter().map(super::Group::text).collect()
}

fn line_text(lines: &[DiffLine]) -> String {
    lines
        .iter()
        .map(|l| match l {
            DiffLine::Gap => "⋯".to_string(),
            DiffLine::Same(t) => format!("  {t}"),
            DiffLine::Removed(t) => format!("- {t}"),
            DiffLine::Added(t) => format!("+ {t}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_summary_reads_what_changed() {
    let before = objects([deployment(
        "web",
        "1.15.0",
        3,
        json!([{"name": "BAR", "value": "a"}, {"name": "BAZ", "value": "x"}]),
    )]);
    let after = objects([deployment(
        "web",
        "1.16.0",
        5,
        json!([{"name": "BAR", "value": "b"}, {"name": "FOO", "value": "on"}]),
    )]);
    let d = compare(&before, &after);
    assert_eq!(
        texts(&d),
        ["image web 1.15.0 → 1.16.0", "replicas 3 → 5", "env BAR: a → b", "env BAZ removed", "env FOO added: on"]
    );
    assert_eq!(d.noise(), 0);
    assert_eq!(d.groups[0].resources, [after.keys().next().unwrap().clone()]);
}

#[test]
fn one_change_in_many_resources_is_one_line_with_the_list() {
    let names: Vec<String> = (0..12).map(|i| format!("worker-{i}")).collect();
    let before = objects(names.iter().map(|n| deployment(n, "1.15.0", 1, json!([]))));
    let after = objects(names.iter().map(|n| deployment(n, "1.16.0", 1, json!([]))));
    let d = compare(&before, &after);
    assert_eq!(texts(&d), ["image web 1.15.0 → 1.16.0"]);
    assert_eq!(d.groups[0].resources.len(), 12);
    assert_eq!(d.resources.len(), 12);
}

#[test]
fn resources_added_and_pruned_are_counted() {
    let cm =
        |name: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}, "data": {"a": "1"}});
    let before = objects([cm("old"), cm("kept")]);
    let after = objects([cm("kept"), cm("new-one"), cm("new-two")]);
    let d = compare(&before, &after);
    assert_eq!(texts(&d), ["2 resources added", "1 resource pruned"]);
    assert_eq!(d.unchanged, 1);
}

#[test]
fn config_keys_are_named_not_dumped() {
    let cm = |data: Value| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg"}, "data": data});
    let d = compare(&objects([cm(json!({"a": "1", "b": "2"}))]), &objects([cm(json!({"a": "1", "b": "3", "c": "4"}))]));
    assert_eq!(texts(&d), ["config key b changed", "config key c added"]);
    // Data added whole is split by key too.
    let d = compare(
        &objects([json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg"}})]),
        &objects([cm(json!({"x": "1"}))]),
    );
    assert_eq!(texts(&d), ["config key x added"]);
}

#[test]
fn noise_is_folded_and_counted() {
    let noisy = |tag: &str, sum: &str, chart: &str, hash: &str, cfg: &str, at: &str| {
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "web", "namespace": "shop",
                "labels": {"helm.sh/chart": chart, "app.kubernetes.io/version": tag},
                "annotations": {"deployment.kubernetes.io/revision": hash.len().to_string(),
                    "example.com/deployed-at": at,
                    "kubectl.kubernetes.io/last-applied-configuration": format!("{{\"tag\":\"{tag}\"}}")},
                "managedFields": [{"manager": "argocd", "time": at}],
                "resourceVersion": hash,
            },
            "spec": {"template": {
                "metadata": {
                    "labels": {"pod-template-hash": hash},
                    "annotations": {"checksum/config": sum},
                },
                "spec": {
                    "containers": [{"name": "app", "image": format!("registry.example.com/shop/web:{tag}")}],
                    "volumes": [{"name": "cfg", "configMap": {"name": cfg}}],
                },
            }},
            "status": {"replicas": hash.len()},
        })
    };
    let before = noisy("1.15.0", "aaa", "web-1.0.0", "7d9f8c6b5", "web-config-7h2k9m4t5c", "2026-01-01T00:00:00Z");
    let after = noisy("1.16.0", "bbb", "web-1.1.0", "5c4b3a2d1x", "web-config-bm6fk7c4hg", "2026-01-02T00:00:00Z");
    let cm = |name: &str, v: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "shop"}, "data": {"k": v}});
    let d = compare(
        &objects([before, cm("web-config-7h2k9m4t5c", "1")]),
        &objects([after, cm("web-config-bm6fk7c4hg", "2")]),
    );
    assert_eq!(
        texts(&d),
        [
            "image web 1.15.0 → 1.16.0",
            "config key k changed",
            "renamed web-config-7h2k9m4t5c → web-config-bm6fk7c4hg (regenerated)"
        ],
        "only the image and the config are news"
    );
    let rules: Vec<&str> = d.noise_by_rule().into_iter().map(|(r, _)| r).collect();
    for rule in [
        "checksum annotation",
        "chart label",
        "version label",
        "generated hash label",
        "field managers",
        "status",
        "last-applied annotation",
        "timestamp",
        rules::GENERATED_NAME,
        "set by the API server",
        "revision annotation",
    ] {
        assert!(rules.contains(&rule), "{rule} folded: {rules:?}");
    }
    let r = d.resources.iter().find(|r| r.key.kind == "Deployment").unwrap();
    let hidden = line_text(r.lines(false));
    assert!(
        hidden.contains("1.16.0") && !hidden.contains("checksum") && !hidden.contains("pod-template-hash"),
        "{hidden}"
    );
    let shown = line_text(r.lines(true));
    assert!(shown.contains("checksum/config") && shown.contains("web-config-bm6fk7c4hg"), "{shown}");
}

#[test]
fn digests_follow_their_tags() {
    let with = |image: &str| deployment_image(image);
    let d = compare(
        &objects([with("registry.example.com/web:1.0@sha256:aaaa")]),
        &objects([with("registry.example.com/web:1.1@sha256:bbbb")]),
    );
    assert_eq!(texts(&d), ["image web 1.0 → 1.1"]);
    assert_eq!(d.noise_by_rule(), [(rules::DIGEST, 1)]);
    // Counted, but on the image's line: it stays in the lines.
    assert!(line_text(d.resources[0].lines(false)).contains("sha256:bbbb"));
    // The digest alone: said, not hidden.
    let d = compare(
        &objects([with("registry.example.com/web:1.0@sha256:aaaa")]),
        &objects([with("registry.example.com/web:1.0@sha256:cccc")]),
    );
    assert_eq!(texts(&d), ["image web 1.0: digest changed"]);
    assert_eq!(d.noise(), 0);
}

fn deployment_image(image: &str) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
        "spec": {"template": {"spec": {"containers": [{"name": "app", "image": image}]}}}})
}

#[test]
fn a_regenerated_config_map_is_the_same_one_renamed() {
    let cm = |name: &str, v: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}, "data": {"k": v}});
    let d = compare(&objects([cm("app-config-7h2k9m4t5c", "1")]), &objects([cm("app-config-bm6fk7c4hg", "2")]));
    assert_eq!(
        texts(&d),
        ["config key k changed", "renamed app-config-7h2k9m4t5c → app-config-bm6fk7c4hg (regenerated)"],
        "not pruned and added"
    );
    assert_eq!(d.noise_by_rule(), [(rules::GENERATED_NAME, 1)]);
    // Only ConfigMaps and Secrets, only kustomize's hash: other kinds are pruned and added.
    let job = |name: &str| json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": name}});
    let d = compare(&objects([job("migrate-7h2k9m4t5c")]), &objects([job("migrate-bm6fk7c4hg")]));
    assert_eq!(texts(&d), ["1 resource added", "1 resource pruned"]);
    let d = compare(&objects([cm("app-config-a1b2c", "1")]), &objects([cm("app-config-d4e5f", "1")]));
    assert_eq!(texts(&d), ["1 resource added", "1 resource pruned"], "not kustomize's hash");
}

#[test]
fn a_tag_that_looks_like_a_hash_or_a_date_is_still_a_deploy() {
    for (from, to) in [
        ("main-a1b2c3d", "main-e4f5a6b"),
        ("1.0.0-beta2", "1.0.0-beta3"),
        ("20260101", "20260102"),
        ("web-7h2k9m4t5c", "web-bm6fk7c4hg"),
    ] {
        let d = compare(
            &objects([deployment_image(&format!("registry.example.com/web:{from}"))]),
            &objects([deployment_image(&format!("registry.example.com/web:{to}"))]),
        );
        assert_eq!(texts(&d), [format!("image web {from} → {to}")]);
        assert_eq!(d.noise(), 0);
    }
}

#[test]
fn hash_like_and_date_values_in_env_and_data_are_changes() {
    let d = compare(
        &objects([deployment(
            "web",
            "1",
            1,
            json!([{"name": "BUILD", "value": "main-a1b2c3d"},
            {"name": "AT", "value": "2026-01-01T00:00:00Z"}]),
        )]),
        &objects([deployment(
            "web",
            "1",
            1,
            json!([{"name": "BUILD", "value": "main-e4f5a6b"},
            {"name": "AT", "value": "2026-01-02T00:00:00Z"}]),
        )]),
    );
    assert_eq!(
        texts(&d),
        ["env AT: 2026-01-01T00:00:00Z → 2026-01-02T00:00:00Z", "env BUILD: main-a1b2c3d → main-e4f5a6b"]
    );
    let cm =
        |v: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg"}, "data": {"ref": v}});
    let d = compare(&objects([cm("app-7h2k9m4t5c")]), &objects([cm("app-bm6fk7c4hg")]));
    assert_eq!(texts(&d), ["config key ref changed"]);
}

#[test]
fn a_restart_asked_for_is_a_change() {
    let at = |t: &str| {
        let mut d = deployment("web", "1", 1, json!([]));
        d["spec"]["template"]["metadata"]["annotations"] = json!({"kubectl.kubernetes.io/restartedAt": t});
        d
    };
    let d = compare(&objects([at("2026-01-01T00:00:00Z")]), &objects([at("2026-01-02T00:00:00Z")]));
    assert_eq!(d.groups.len(), 1);
    assert!(texts(&d)[0].contains("restartedAt"), "{:?}", texts(&d));
}

#[test]
fn a_pod_template_with_only_noise_changed_still_restarts() {
    // Over Argo CD's API, a Secret's values are `+` runs: the checksum is all that says it changed.
    let secret = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db"}, "data": {"pw": "++++++++"}});
    let web = |sum: &str| {
        let mut d = deployment("web", "1", 2, json!([]));
        d["spec"]["template"]["metadata"]["annotations"] = json!({"checksum/secret": sum});
        d
    };
    let d = compare(&objects([web("aaa"), secret.clone()]), &objects([web("bbb"), secret]));
    assert_eq!(texts(&d), ["pod template changed (pods restart): checksum/secret"]);
    assert_eq!(d.noise(), 1, "still counted");
    assert!(line_text(d.resources[0].lines(false)).contains("checksum/secret"), "and shown");
    // With a fact in the template (an image), the checksum stays folded.
    let mut bumped = web("bbb");
    bumped["spec"]["template"]["spec"]["containers"][0]["image"] = json!("registry.example.com/shop/web:2");
    let d = compare(&objects([web("aaa")]), &objects([bumped]));
    assert_eq!(texts(&d), ["image web 1 → 2"]);
    // Noise outside the template is no restart.
    let mut labelled = web("aaa");
    labelled["metadata"]["labels"]["helm.sh/chart"] = json!("web-2");
    let d = compare(&objects([web("aaa")]), &objects([labelled]));
    assert!(d.groups.is_empty());
}

#[test]
fn a_list_counts_as_its_items() {
    let list = json!({"apiVersion": "v1", "kind": "SecretList", "items": [
        {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db"}, "data": {"k": "value-one"}}]});
    let mut changed = list.clone();
    changed["items"][0]["data"]["k"] = json!("value-two");
    let d = compare(&objects([list]), &objects([changed]));
    assert_eq!(texts(&d), ["Secret key k changed"]);
    assert!(!line_text(d.resources[0].lines(true)).contains("value-"));
}

#[test]
fn a_secret_never_shows_its_values() {
    let secret = |data: Value| {
        json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "annotations": {
            "kubectl.kubernetes.io/last-applied-configuration": data.to_string()}}, "data": data})
    };
    let before = secret(json!({"alpha": "first-old", "beta": "kept", "gone": "dropped"}));
    let after = secret(json!({"alpha": "first-new", "beta": "kept", "fresh": "added-value"}));
    let d = compare(&objects([before]), &objects([after]));
    assert_eq!(texts(&d), ["Secret key alpha changed", "Secret key fresh added", "Secret key gone removed"]);
    let all = format!("{}\n{}\n{:?}", line_text(d.resources[0].lines(true)), texts(&d).join("\n"), d.groups);
    for value in ["first-old", "first-new", "kept", "added-value", "dropped"] {
        assert!(!all.contains(value), "{value} shown:\n{all}");
    }
    // Argo CD's own masks (runs of `+`) say nothing about a change.
    let d = compare(&objects([secret(json!({"a": "++++++"}))]), &objects([secret(json!({"a": "++++++++"}))]));
    assert!(d.groups.is_empty(), "{:?}", texts(&d));
}

#[test]
fn helm_values_read_like_the_resources() {
    let before = json!({"image": {"repository": "registry.example.com/shop/web", "tag": "1.15.0"}, "replicaCount": 2,
        "ingress": {"enabled": false}});
    let after = json!({"image": {"repository": "registry.example.com/shop/web", "tag": "1.16.0"}, "replicaCount": 4,
        "ingress": {"enabled": true, "host": "web.example.com"}});
    let d = compare_values(&before, &after);
    assert_eq!(
        texts(&d),
        [
            "image web 1.15.0 → 1.16.0",
            "replicas 2 → 4",
            "ingress.enabled: false → true",
            "ingress.host added: web.example.com"
        ]
    );
    let lines = d.resources[0].lines(false);
    assert!(lines.iter().any(|l| matches!(l, DiffLine::Removed(t) if t.contains("tag: ") && t.contains("1.15.0"))));
    assert!(lines.iter().any(|l| matches!(l, DiffLine::Added(t) if t.contains("tag: ") && t.contains("1.16.0"))));
    assert!(compare_values(&before, &before).is_empty());
    assert!(compare_values(&Value::Null, &Value::Null).is_empty());
}

#[test]
fn images_and_paths_are_read() {
    assert_eq!(parse_image("registry:5000/team/web:1.2"), ("registry:5000/team/web", Some("1.2"), None));
    assert_eq!(parse_image("web@sha256:abc"), ("web", None, Some("sha256:abc")));
    assert_eq!(parse_image("web:1@sha256:abc"), ("web", Some("1"), Some("sha256:abc")));
    let path =
        [Seg::Key("spec".into()), Seg::Key("containers".into()), Seg::Item("app".into()), Seg::Key("image".into())];
    assert_eq!(path_text(&path), "spec.containers[app].image");
    assert_eq!(kustomize_stem("app-config-7h2k9m4t5c"), Some("app-config"));
    assert_eq!(kustomize_stem("web-7d9f8c6b5"), None, "nine characters");
    assert_eq!(kustomize_stem("web-a1b2c3d4e5"), None, "not kustomize's alphabet");
    assert_eq!(kustomize_stem("web-frontends"), None);
    assert!(rules::matches(
        &["**", "labels", "helm.sh/chart"],
        &[Seg::Key("metadata".into()), Seg::Key("labels".into()), Seg::Key("helm.sh/chart".into())]
    ));
    assert!(!rules::matches(&["status", "**"], &[Seg::Key("spec".into()), Seg::Key("status".into())]));
}

// MARK: Properties

/// Keys no noise rule names (they all start with `k`), values that are no timestamp, hash or
/// image.
fn tree(depth: u32) -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        (0..100i64).prop_map(|n| json!(n)),
        "[a-z]{1,8}".prop_map(Value::String),
        any::<bool>().prop_map(Value::Bool)
    ];
    leaf.prop_recursive(depth, 24, 4, |inner| {
        prop_oneof![
            prop::collection::btree_map("k[a-z]{1,4}", inner.clone(), 1..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
            prop::collection::vec(inner, 1..3).prop_map(Value::Array),
        ]
    })
    .boxed()
}

fn with_spec(spec: Value) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop"}, "spec": spec})
}

/// Noise on the usual places: checksum, chart label, hash label, managed fields, status.
fn add_noise(obj: &mut Value, n: u32) {
    obj["metadata"]["labels"] =
        json!({"helm.sh/chart": format!("web-1.{n}.0"), "pod-template-hash": format!("abc{n}de9")});
    obj["metadata"]["annotations"] = json!({"checksum/config": format!("sum{n}")});
    obj["metadata"]["managedFields"] = json!([{"manager": format!("m{n}")}]);
    obj["status"] = json!({"replicas": n});
}

proptest! {
    /// However much noise changes along with it, a real change is never folded away: it is a
    /// fact, and it stays in the lines shown with noise hidden.
    #[test]
    fn noise_never_hides_a_real_change(spec in tree(3), other in tree(3), n1 in 0u32..50, n2 in 50u32..100) {
        prop_assume!(spec != other);
        let mut before = with_spec(spec);
        let mut after = with_spec(other);
        add_noise(&mut before, n1);
        add_noise(&mut after, n2);
        let d = compare(&objects([before]), &objects([after]));
        prop_assert!(!d.groups.is_empty(), "the change is a fact");
        prop_assert!(d.noise() >= 4, "the noise is counted: {:?}", d.noise_by_rule());
        let r = &d.resources[0];
        prop_assert!(r.lines(false).iter().any(|l| matches!(l, DiffLine::Added(_) | DiffLine::Removed(_))));
        let hidden = line_text(r.lines(false));
        prop_assert!(!hidden.contains("checksum/config") && !hidden.contains("helm.sh/chart"), "{}", hidden);
        // Every leaf off the noise paths is a fact's.
        for leaf in leaves(&d.resources[0].before, &d.resources[0].after) {
            let noise = rules::noise_of(&r.key, &leaf, &super::Renames::new()).is_some();
            prop_assert!(noise || leaf.path.first() == Some(&Seg::Key("spec".into())), "{:?}", leaf.path);
        }
    }

    /// Two values trees differ exactly when their smart diff says something; never noise.
    #[test]
    fn values_differ_when_the_diff_says_so(a in tree(3), b in tree(3)) {
        let d = compare_values(&a, &b);
        prop_assert_eq!(d.is_empty(), a == b);
        prop_assert_eq!(d.noise(), 0);
        prop_assert_eq!(d.groups.is_empty(), a == b);
    }

    /// Grouping keeps every change of every resource, each once, in one group.
    #[test]
    fn grouping_never_loses_a_change(
        picks in prop::collection::vec((0usize..6, prop::collection::vec(0usize..8, 0..5)), 0..12)
    ) {
        let pool = [
            Fact::Image { from_repo: "r/web".into(), from_tag: "1".into(), to_repo: "r/web".into(), to_tag: "2".into() },
            Fact::Replicas { from: Some("1".into()), to: Some("2".into()) },
            Fact::Env { name: "A".into(), edit: Edit::Added, from: None, to: Some("x".into()) },
            Fact::SecretKey { key: "p".into(), edit: Edit::Changed },
            Fact::Added,
            Fact::Pruned,
            Fact::Field { path: "spec.x".into(), edit: Edit::Changed, from: Some("a".into()), to: Some("b".into()) },
            Fact::ConfigKey { key: "c".into(), edit: Edit::Removed },
        ];
        let resources: Vec<ResourceChange> = picks
            .iter()
            .map(|(name, facts)| {
                let key = ObjectKey { kind: "Deployment".into(), name: format!("w{name}"), ..Default::default() };
                let mut fs: Vec<Fact> = Vec::new();
                for f in facts {
                    if !fs.contains(&pool[*f]) {
                        fs.push(pool[*f].clone());
                    }
                }
                ResourceChange::new(key, fs, Vec::new(), Value::Null, Value::Null)
            })
            .collect();
        let groups = group(&resources);
        let mut pairs = std::collections::BTreeSet::new();
        for r in &resources {
            for f in &r.facts {
                pairs.insert((r.key.clone(), f.clone()));
            }
        }
        let mut grouped = std::collections::BTreeSet::new();
        for g in &groups {
            for k in &g.resources {
                prop_assert!(grouped.insert((k.clone(), g.fact.clone())), "each pair once");
            }
        }
        prop_assert_eq!(grouped, pairs);
        // Each fact in one group only.
        let facts: std::collections::BTreeSet<&Fact> = groups.iter().map(|g| &g.fact).collect();
        prop_assert_eq!(facts.len(), groups.len());
    }

    /// A Secret's values, whatever they are and however they change, never show.
    #[test]
    fn secret_values_never_show(
        before in prop::collection::btree_map("[a-c]", "v[a-z]{6}", 0..4),
        after in prop::collection::btree_map("[a-c]", "v[a-z]{6}", 0..4),
        string_data in any::<bool>(),
    ) {
        let field = if string_data { "stringData" } else { "data" };
        let secret = |data: &std::collections::BTreeMap<String, String>| {
            let mut s = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "s",
                "annotations": {"kubectl.kubernetes.io/last-applied-configuration": json!(data).to_string()}}});
            s[field] = json!(data);
            s
        };
        let d = compare(&objects([secret(&before)]), &objects([secret(&after)]));
        let mut shown = texts(&d).join("\n");
        for r in &d.resources {
            shown.push_str(&line_text(r.lines(true)));
            shown.push_str(&format!("{:?}", r.facts));
        }
        for v in before.values().chain(after.values()) {
            prop_assert!(!shown.contains(v.as_str()), "{} shown", v);
        }
        // Each key that changed is said.
        for k in before.keys().chain(after.keys()) {
            if before.get(k) != after.get(k) {
                prop_assert!(shown.contains(&format!("Secret key {k} ")), "{} not said: {}", k, shown);
            }
        }
    }

    /// A tag is never noise, however much it looks like a hash, a generated name or a date, and
    /// whatever noise changes along with it.
    #[test]
    fn a_tag_is_never_noise(
        from in "(main-[0-9a-f]{7}|[a-z]{3}-[bcdfghkmt2456789]{10}|20[0-9]{6}|1\\.0\\.0-beta[0-9])",
        to in "(main-[0-9a-f]{7}|[a-z]{3}-[bcdfghkmt2456789]{10}|20[0-9]{6}|1\\.0\\.0-beta[0-9])",
        n1 in 0u32..50,
        n2 in 50u32..100,
    ) {
        prop_assume!(from != to);
        let mut before = deployment_image(&format!("registry.example.com/web:{from}"));
        let mut after = deployment_image(&format!("registry.example.com/web:{to}"));
        add_noise(&mut before, n1);
        add_noise(&mut after, n2);
        let d = compare(&objects([before]), &objects([after]));
        prop_assert_eq!(texts(&d), [format!("image web {from} → {to}")]);
    }

    /// Env values and ConfigMap data that look like timestamps or hashes are changes.
    #[test]
    fn timestamps_and_hashes_in_env_and_data_are_changes(
        s1 in 1_600_000_000i64..1_900_000_000,
        s2 in 1_600_000_000i64..1_900_000_000,
        h1 in "[a-z]{3}-[bcdfghkmt2456789]{10}",
        h2 in "[a-z]{3}-[bcdfghkmt2456789]{10}",
    ) {
        let at = |s: i64| jiff::Timestamp::from_second(s).unwrap().to_string();
        let env = |s: i64, h: &str| json!([{"name": "AT", "value": at(s)}, {"name": "REF", "value": h}]);
        let d = compare(
            &objects([deployment("web", "1", 1, env(s1, &h1))]),
            &objects([deployment("web", "1", 1, env(s2, &h2))]),
        );
        prop_assert_eq!(d.groups.len(), usize::from(s1 != s2) + usize::from(h1 != h2));
        prop_assert_eq!(d.noise(), 0);
        let cm = |h: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "c"}, "data": {"at": h}});
        let d = compare(&objects([cm(&h1)]), &objects([cm(&h2)]));
        prop_assert_eq!(d.groups.is_empty(), h1 == h2);
    }

    /// A pod template where only noise changed (checksums, chart labels) still says its pods
    /// restart.
    #[test]
    fn a_noise_only_pod_template_change_is_said(
        sums in prop::collection::btree_map("checksum/[a-z]{1,6}", "[a-f0-9]{8}", 1..4),
        other in "[a-f0-9]{8}",
        chart in any::<bool>(),
    ) {
        let web = |sums: &std::collections::BTreeMap<String, String>, label: &str| {
            let mut d = deployment("web", "1", 1, json!([]));
            d["spec"]["template"]["metadata"]["annotations"] = json!(sums);
            d["spec"]["template"]["metadata"]["labels"]["helm.sh/chart"] = json!(label);
            d
        };
        let mut changed = sums.clone();
        let first = changed.keys().next().unwrap().clone();
        prop_assume!(changed[&first] != other);
        changed.insert(first, other);
        let d = compare(
            &objects([web(&sums, "web-1")]),
            &objects([web(&changed, if chart { "web-2" } else { "web-1" })]),
        );
        prop_assert_eq!(d.groups.len(), 1);
        prop_assert!(texts(&d)[0].starts_with("pod template changed (pods restart): checksum/"), "{:?}", texts(&d));
    }
}
