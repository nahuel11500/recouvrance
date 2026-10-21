//! Starting points for new objects: built-in templates plus the YAML files in
//! `~/.config/recouvrance/templates/` (one template per file, named after it).
//! `${NAMESPACE}` in a template is replaced by the current namespace.

use std::path::PathBuf;

pub struct Template {
    pub name: String,
    pub body: String,
    /// From the user's folder.
    pub user: bool,
}

const BUILTIN: &[(&str, &str)] = &[
    (
        "Deployment",
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: ${NAMESPACE}\n  labels:\n    app: web\nspec:\n  replicas: 2\n  selector:\n    matchLabels:\n      app: web\n  template:\n    metadata:\n      labels:\n        app: web\n    spec:\n      containers:\n        - name: web\n          image: nginx:1.27\n          ports:\n            - containerPort: 80\n          resources:\n            requests:\n              cpu: 50m\n              memory: 64Mi\n            limits:\n              memory: 128Mi\n          readinessProbe:\n            httpGet:\n              path: /\n              port: 80\n",
    ),
    (
        "Service",
        "apiVersion: v1\nkind: Service\nmetadata:\n  name: web\n  namespace: ${NAMESPACE}\nspec:\n  selector:\n    app: web\n  ports:\n    - name: http\n      port: 80\n      targetPort: 80\n",
    ),
    (
        "Ingress",
        "apiVersion: networking.k8s.io/v1\nkind: Ingress\nmetadata:\n  name: web\n  namespace: ${NAMESPACE}\nspec:\n  ingressClassName: nginx\n  rules:\n    - host: web.example.com\n      http:\n        paths:\n          - path: /\n            pathType: Prefix\n            backend:\n              service:\n                name: web\n                port:\n                  number: 80\n",
    ),
    (
        "ConfigMap",
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: settings\n  namespace: ${NAMESPACE}\ndata:\n  LOG_LEVEL: info\n",
    ),
    (
        "Secret",
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: credentials\n  namespace: ${NAMESPACE}\ntype: Opaque\nstringData:\n  username: admin\n  password: change-me\n",
    ),
    (
        "Job",
        "apiVersion: batch/v1\nkind: Job\nmetadata:\n  name: once\n  namespace: ${NAMESPACE}\nspec:\n  backoffLimit: 2\n  ttlSecondsAfterFinished: 3600\n  template:\n    spec:\n      restartPolicy: Never\n      containers:\n        - name: main\n          image: busybox:1.37\n          command: [sh, -c, \"echo hello\"]\n",
    ),
    (
        "CronJob",
        "apiVersion: batch/v1\nkind: CronJob\nmetadata:\n  name: nightly\n  namespace: ${NAMESPACE}\nspec:\n  schedule: \"0 3 * * *\"\n  concurrencyPolicy: Forbid\n  jobTemplate:\n    spec:\n      template:\n        spec:\n          restartPolicy: OnFailure\n          containers:\n            - name: main\n              image: busybox:1.37\n              command: [sh, -c, \"date\"]\n",
    ),
    (
        "PersistentVolumeClaim",
        "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: data\n  namespace: ${NAMESPACE}\nspec:\n  accessModes: [ReadWriteOnce]\n  resources:\n    requests:\n      storage: 10Gi\n",
    ),
    (
        "HorizontalPodAutoscaler",
        "apiVersion: autoscaling/v2\nkind: HorizontalPodAutoscaler\nmetadata:\n  name: web\n  namespace: ${NAMESPACE}\nspec:\n  scaleTargetRef:\n    apiVersion: apps/v1\n    kind: Deployment\n    name: web\n  minReplicas: 2\n  maxReplicas: 10\n  metrics:\n    - type: Resource\n      resource:\n        name: cpu\n        target:\n          type: Utilization\n          averageUtilization: 70\n",
    ),
    (
        "PodDisruptionBudget",
        "apiVersion: policy/v1\nkind: PodDisruptionBudget\nmetadata:\n  name: web\n  namespace: ${NAMESPACE}\nspec:\n  minAvailable: 1\n  selector:\n    matchLabels:\n      app: web\n",
    ),
    (
        "NetworkPolicy (deny all ingress)",
        "apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: deny-all-ingress\n  namespace: ${NAMESPACE}\nspec:\n  podSelector: {}\n  policyTypes: [Ingress]\n",
    ),
    (
        "ServiceAccount + Role + RoleBinding",
        "apiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: reader\n  namespace: ${NAMESPACE}\n---\napiVersion: rbac.authorization.k8s.io/v1\nkind: Role\nmetadata:\n  name: reader\n  namespace: ${NAMESPACE}\nrules:\n  - apiGroups: [\"\"]\n    resources: [pods, pods/log]\n    verbs: [get, list, watch]\n---\napiVersion: rbac.authorization.k8s.io/v1\nkind: RoleBinding\nmetadata:\n  name: reader\n  namespace: ${NAMESPACE}\nsubjects:\n  - kind: ServiceAccount\n    name: reader\n    namespace: ${NAMESPACE}\nroleRef:\n  apiGroup: rbac.authorization.k8s.io\n  kind: Role\n  name: reader\n",
    ),
    ("Namespace", "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: my-namespace\n"),
];

pub fn folder() -> PathBuf {
    crate::mcp::config_dir().join("templates")
}

/// Put the namespace into a template: every `${NAMESPACE}`, and (for templates written
/// before the placeholder existed) a bare `NAMESPACE` that is the whole value of a
/// `namespace:` key. `NAMESPACE` anywhere else (e.g. `POD_NAMESPACE`) is left alone.
pub fn substitute(body: &str, namespace: &str) -> String {
    static LEGACY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?m)^([ \t]*(?:-[ \t]+)?namespace:[ \t]*)("NAMESPACE"|'NAMESPACE'|NAMESPACE)([ \t]*(?:#.*)?)$"#,
        )
        .expect("valid regex")
    });
    let body = body.replace("${NAMESPACE}", namespace);
    LEGACY
        .replace_all(&body, |c: &regex::Captures| {
            let q = if c[2].starts_with(['"', '\'']) { &c[2][..1] } else { "" };
            format!("{}{q}{namespace}{q}{}", &c[1], &c[3])
        })
        .into_owned()
}

/// Built-ins first, then the user's files (sorted by name), with the namespace put in.
pub fn all(namespace: &str) -> Vec<Template> {
    let mut out: Vec<Template> = BUILTIN
        .iter()
        .map(|(n, b)| Template { name: n.to_string(), body: substitute(b, namespace), user: false })
        .collect();
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
        .collect();
    files.sort();
    for p in files {
        if let Ok(body) = std::fs::read_to_string(&p) {
            let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            out.push(Template { name, body: substitute(&body, namespace), user: true });
        }
    }
    out
}

/// Create the folder with a README-like example, so it is easy to start.
pub fn ensure_folder() -> std::io::Result<PathBuf> {
    let dir = folder();
    std::fs::create_dir_all(&dir)?;
    let example = dir.join("example-redis.yaml");
    if std::fs::read_dir(&dir)?.next().is_none() {
        std::fs::write(
            example,
            "# Your own template: any YAML file in this folder shows up under Templates.\n# ${NAMESPACE} is replaced by the namespace you are in.\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: redis\n  namespace: ${NAMESPACE}\nspec:\n  replicas: 1\n  selector:\n    matchLabels: {app: redis}\n  template:\n    metadata:\n      labels: {app: redis}\n    spec:\n      containers:\n        - name: redis\n          image: redis:7\n",
        )?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtins_parse() {
        for t in super::all("team-a").into_iter().filter(|t| !t.user) {
            let docs = crate::yaml::documents(&t.body).unwrap_or_else(|e| panic!("{}: {e}", t.name));
            for v in docs.into_iter().filter(|v| !v.is_null()) {
                assert!(v["kind"].is_string(), "{}", t.name);
                if v["kind"] != "Namespace" {
                    assert_eq!(v.pointer("/metadata/namespace").and_then(|n| n.as_str()), Some("team-a"), "{}", t.name);
                }
            }
        }
    }

    #[test]
    fn substitution_is_delimited() {
        use super::substitute;
        let body = "metadata:\n  namespace: ${NAMESPACE}\nenv:\n  - name: POD_NAMESPACE\n    value: ${NAMESPACE}-x\n";
        assert_eq!(
            substitute(body, "team-a"),
            "metadata:\n  namespace: team-a\nenv:\n  - name: POD_NAMESPACE\n    value: team-a-x\n"
        );
        // Older templates: a bare NAMESPACE only as the whole value of a namespace key.
        let old = "metadata:\n  namespace: NAMESPACE\nsubjects:\n  - namespace: \"NAMESPACE\" # here\nenv:\n  - name: POD_NAMESPACE\n    value: MY_NAMESPACE\n  namespace: NAMESPACE_X\n";
        assert_eq!(
            substitute(old, "b"),
            "metadata:\n  namespace: b\nsubjects:\n  - namespace: \"b\" # here\nenv:\n  - name: POD_NAMESPACE\n    value: MY_NAMESPACE\n  namespace: NAMESPACE_X\n"
        );
    }
}
