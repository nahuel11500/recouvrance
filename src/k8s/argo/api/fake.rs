//! A fake Argo CD API server for tests: the routes the app reads and writes (list, stream,
//! one Application, its managed resources, sync, terminate, rollback, refresh, projects,
//! clusters), over plain http on this machine. What a write asks is kept in the Application
//! as argocd-server would put it (its `operation`), and every change is sent on the stream;
//! the test plays Argo CD's controller with [`Fake::set`].

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// The fake server's state.
pub struct Fake {
    apps: Mutex<Vec<Value>>,
    /// Each Application's managed resources, by name.
    managed: Mutex<std::collections::HashMap<String, Value>>,
    /// Every request but reads of the stream: method, path and query, body.
    requests: Mutex<Vec<(String, String, Value)>>,
    events: tokio::sync::broadcast::Sender<String>,
    version: Mutex<u64>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `patch` merged into `v`, as a JSON merge patch: null removes.
pub fn merge(v: &mut Value, patch: &Value) {
    match (v.as_object_mut(), patch.as_object()) {
        (Some(obj), Some(p)) => {
            for (k, pv) in p {
                if pv.is_null() {
                    obj.remove(k);
                } else {
                    merge(obj.entry(k.clone()).or_insert(Value::Null), pv);
                }
            }
        }
        _ => *v = patch.clone(),
    }
}

impl Fake {
    /// Serve `apps` on a free port of this machine: the server's address, and the task that
    /// serves it (dropped, it stops).
    pub async fn start(apps: Vec<Value>) -> anyhow::Result<(Arc<Self>, String, crate::k8s::TaskGuard)> {
        let fake = Arc::new(Self {
            apps: Mutex::new(apps),
            managed: Mutex::default(),
            requests: Mutex::default(),
            events: tokio::sync::broadcast::channel(256).0,
            version: Mutex::new(1),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let router = axum::Router::new().fallback(handle).with_state(fake.clone());
        let task = crate::k8s::spawn_guarded(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((fake, base, task))
    }

    /// The Application `name` as kept now.
    pub fn get(&self, name: &str) -> Option<Value> {
        lock(&self.apps).iter().find(|a| a.pointer("/metadata/name") == Some(&json!(name))).cloned()
    }

    /// Change the Application `name` (a merge patch), as Argo CD's controller would.
    pub fn set(&self, name: &str, patch: &Value) {
        let changed = {
            let mut apps = lock(&self.apps);
            let Some(app) = apps.iter_mut().find(|a| a.pointer("/metadata/name") == Some(&json!(name))) else {
                return;
            };
            merge(app, patch);
            self.bump(app);
            app.clone()
        };
        self.send(&changed);
    }

    /// What its managed-resources route answers for `name`.
    pub fn set_managed(&self, name: &str, managed: Value) {
        lock(&self.managed).insert(name.to_string(), managed);
    }

    /// The requests that changed something (sync, terminate, rollback, refresh).
    pub fn writes(&self) -> Vec<(String, String, Value)> {
        lock(&self.requests).iter().filter(|(m, p, _)| m != "GET" || p.contains("refresh=")).cloned().collect()
    }

    fn bump(&self, app: &mut Value) {
        let mut v = lock(&self.version);
        *v += 1;
        app["metadata"]["resourceVersion"] = json!(v.to_string());
    }

    fn send(&self, app: &Value) {
        let event = json!({"result": {"type": "MODIFIED", "application": app}});
        let _ = self.events.send(format!("data: {event}\n\n"));
    }

    /// The Application named by `path`'s `/api/v1/applications/{name}`, changed by `f`, which
    /// may refuse it (an HTTP status and a message).
    fn change(&self, name: &str, f: impl FnOnce(&mut Value) -> Result<(), (StatusCode, String)>) -> Response {
        let changed = {
            let mut apps = lock(&self.apps);
            let Some(app) = apps.iter_mut().find(|a| a.pointer("/metadata/name") == Some(&json!(name))) else {
                return (StatusCode::NOT_FOUND, json!({"message": "not found"}).to_string()).into_response();
            };
            if let Err((status, message)) = f(app) {
                return (status, json!({"message": message}).to_string()).into_response();
            }
            self.bump(app);
            app.clone()
        };
        self.send(&changed);
        json_response(&changed)
    }
}

fn json_response(v: &Value) -> Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

async fn handle(State(fake): State<Arc<Fake>>, method: Method, uri: Uri, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if path != "/api/v1/stream/applications" {
        let full = if query.is_empty() { path.clone() } else { format!("{path}?{query}") };
        lock(&fake.requests).push((method.to_string(), full, body.clone()));
    }
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["api", "v1", "session", "userinfo"]) => json_response(&json!({"loggedIn": true, "username": "e2e"})),
        ("GET", ["api", "version"]) => json_response(&json!({"Version": "v0.0.0-fake"})),
        ("GET", ["api", "v1", "clusters"]) => json_response(&json!({"items": []})),
        ("GET", ["api", "v1", "projects"]) => json_response(&json!({"items": []})),
        ("GET", ["api", "v1", "applications"]) => {
            let v = lock(&fake.version).to_string();
            json_response(&json!({"metadata": {"resourceVersion": v}, "items": *lock(&fake.apps)}))
        }
        ("GET", ["api", "v1", "stream", "applications"]) => {
            let rx = fake.events.subscribe();
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(s) => return Some((Ok::<_, Infallible>(Bytes::from(s)), rx)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            });
            // A first comment, so the answer's head goes at once.
            let first = futures::stream::once(async { Ok::<_, Infallible>(Bytes::from(": open\n\n")) });
            let body = Body::from_stream(futures::StreamExt::chain(first, stream));
            ([("content-type", "text/event-stream")], body).into_response()
        }
        ("GET", ["api", "v1", "applications", name, "managed-resources"]) => {
            json_response(lock(&fake.managed).get(*name).unwrap_or(&json!({"items": []})))
        }
        ("GET", ["api", "v1", "applications", name]) if query.contains("refresh=") => {
            let mode = if query.contains("refresh=hard") { "hard" } else { "normal" };
            fake.change(name, |app| {
                app["metadata"]["annotations"]["argocd.argoproj.io/refresh"] = json!(mode);
                Ok(())
            })
        }
        ("GET", ["api", "v1", "applications", name]) => match fake.get(name) {
            Some(app) => json_response(&app),
            None => (StatusCode::NOT_FOUND, json!({"message": "not found"}).to_string()).into_response(),
        },
        ("POST", ["api", "v1", "applications", name, "sync"]) => fake.change(name, |app| {
            if app.get("operation").is_some_and(|o| !o.is_null()) {
                return Err((StatusCode::BAD_REQUEST, "another operation is already in progress".into()));
            }
            let mut sync = json!({"prune": body["prune"], "dryRun": body["dryRun"], "syncStrategy": body["strategy"]});
            for key in ["resources", "revision", "syncOptions"] {
                if let Some(v) = body.get(key) {
                    sync[key] = v.clone();
                }
            }
            app["operation"] = json!({"initiatedBy": {"username": "e2e"}, "sync": sync});
            Ok(())
        }),
        ("POST", ["api", "v1", "applications", name, "rollback"]) => fake.change(name, |app| {
            if app.get("operation").is_some_and(|o| !o.is_null()) {
                return Err((StatusCode::BAD_REQUEST, "another operation is already in progress".into()));
            }
            let id = body["id"].as_i64();
            let history = app.pointer("/status/history").and_then(Value::as_array).cloned().unwrap_or_default();
            let Some(entry) = history.iter().find(|e| e["id"].as_i64() == id) else {
                return Err((StatusCode::BAD_REQUEST, "no such deployment".into()));
            };
            let sync = json!({"revision": entry["revision"], "source": entry["source"],
                "dryRun": body.get("dryRun").cloned().unwrap_or(json!(false)), "syncStrategy": {"apply": {}}});
            app["operation"] = json!({"initiatedBy": {"username": "e2e"}, "sync": sync});
            Ok(())
        }),
        ("DELETE", ["api", "v1", "applications", name, "operation"]) => fake.change(name, |app| {
            if app.get("operation").is_none_or(Value::is_null) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Unable to terminate operation. No operation is in progress".into(),
                ));
            }
            app["status"]["operationState"]["phase"] = json!("Terminating");
            Ok(())
        }),
        ("DELETE", ["api", "v1", "applications", name]) => {
            let gone = {
                let mut apps = lock(&fake.apps);
                let before = apps.len();
                apps.retain(|a| a.pointer("/metadata/name") != Some(&json!(name)));
                apps.len() < before
            };
            if gone {
                json_response(&json!({}))
            } else {
                (StatusCode::NOT_FOUND, json!({"message": "not found"}).to_string()).into_response()
            }
        }
        _ => (StatusCode::NOT_FOUND, json!({"message": format!("{method} {path}: no such route")}).to_string())
            .into_response(),
    }
}
