//! Active port-forwards, owned by the app so they outlive the details panel.

use futures::StreamExt as _;
use gpui_kit::*;
use kube::Client;

use crate::k8s::{self, ForwardMsg, TaskGuard};
use crate::settings::{self, SavedForward};

pub struct Forward {
    pub id: usize,
    pub label: String,
    pub remote: u16,
    pub local: Option<u16>,
    pub connections: usize,
    pub error: Option<String>,
    /// How to bring it back after a restart.
    pub saved: SavedForward,
    _session: (TaskGuard, Task<()>),
}

impl Forward {
    /// Where to reach it from a browser.
    pub fn url(&self) -> Option<String> {
        let port = self.local?;
        let host = match self.saved.address.as_deref() {
            None | Some("127.0.0.1" | "0.0.0.0" | "::" | "") => "localhost".to_string(),
            Some(a) => bracketed(a),
        };
        Some(format!("{}://{host}:{port}", if self.saved.https { "https" } else { "http" }))
    }

    /// "localhost:8080", or with the listen address when it is not loopback.
    pub fn address(&self) -> Option<String> {
        let port = self.local?;
        Some(match self.saved.address.as_deref() {
            None | Some("127.0.0.1") => format!("localhost:{port}"),
            Some(a) => format!("{}:{port}", bracketed(a)),
        })
    }
}

/// An IPv6 address in brackets, so a port can follow it: `[::1]:8080`.
fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
}

/// Ports and port names that serve web pages (UIs, APIs, dashboards).
const WEB_PORTS: [u16; 14] = [80, 443, 3000, 3001, 4200, 5000, 5173, 8000, 8080, 8081, 8443, 8888, 9000, 9090];

/// Whether a one-click forward opens the browser: web ports or names (http, web, ui…),
/// when the setting is on. Databases, gRPC and the like stay quiet.
pub fn opens_browser(port: u16, name: Option<&str>) -> bool {
    if !settings::with(|s| s.open_browser_on_forward) {
        return false;
    }
    let name = name.unwrap_or_default().to_lowercase();
    if ["grpc", "metrics", "db", "sql", "redis", "amqp", "kafka"].iter().any(|n| name.contains(n)) {
        return false;
    }
    WEB_PORTS.contains(&port) || ["http", "web", "ui", "dashboard", "console", "admin"].iter().any(|n| name.contains(n))
}

pub fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    spawn_reaped(std::process::Command::new(opener).arg(url).stdin(std::process::Stdio::null()));
}

/// Start a helper program without waiting for it, reaped in the background so it never
/// lingers as a zombie process.
pub fn spawn_reaped(cmd: &mut std::process::Command) {
    if let Ok(mut child) = cmd.spawn() {
        let _ = std::thread::Builder::new().name("reap".into()).spawn(move || child.wait());
    }
}

#[derive(Default)]
pub struct Forwards {
    pub items: Vec<Forward>,
    next_id: usize,
}

impl Forwards {
    /// Forward `saved.remote` on `saved.pod`; remembered until stopped.
    pub fn start(&mut self, client: Client, saved: SavedForward, cx: &mut Context<Self>) {
        let remote = saved.remote;
        // The saved local port, else the same port when unprivileged and free; the backend reports the real one.
        let wanted = saved
            .local
            .filter(|p| !self.items.iter().any(|f| f.local == Some(*p)))
            .unwrap_or(if remote >= 1024 && !self.items.iter().any(|f| f.local == Some(remote)) { remote } else { 0 });
        let id = self.next_id;
        self.next_id += 1;
        let address = saved.address.clone().unwrap_or_else(|| "127.0.0.1".into());
        let (guard, mut rx) = k8s::port_forward(
            client,
            saved.namespace.clone(),
            saved.pod.clone(),
            saved.service.clone(),
            address,
            wanted,
            remote,
        );
        let task = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    if let Some(f) = this.items.iter_mut().find(|f| f.id == id) {
                        match msg {
                            ForwardMsg::Listening(port) => {
                                f.local = Some(port);
                                f.saved.local = Some(port);
                                let saved = f.saved.clone();
                                if saved.open_browser
                                    && let Some(url) = f.url()
                                {
                                    open_url(&url);
                                }
                                remember(saved);
                            }
                            ForwardMsg::Connection => {
                                f.connections += 1;
                                f.error = None;
                            }
                            ForwardMsg::Error(e) => f.error = Some(e),
                            ForwardMsg::Repointed(pod) => {
                                f.label = match &f.saved.service {
                                    Some(svc) => format!("{}/svc/{svc} → {pod}", f.saved.namespace),
                                    None => format!("{}/{pod}", f.saved.namespace),
                                };
                                f.saved.pod = pod;
                                remember(f.saved.clone());
                            }
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let label = match &saved.service {
            Some(svc) => format!("{}/svc/{svc} → {}", saved.namespace, saved.pod),
            None => format!("{}/{}", saved.namespace, saved.pod),
        };
        self.items.push(Forward {
            id,
            label,
            remote,
            local: None,
            connections: 0,
            error: None,
            saved,
            _session: (guard, task),
        });
        cx.notify();
    }

    /// Stop and forget it (it won't be offered for restore).
    pub fn stop(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(f) = self.items.iter().find(|f| f.id == id) {
            let s = f.saved.clone();
            settings::update(|st| st.saved_forwards.retain(|x| !same(x, &s)));
        }
        self.items.retain(|f| f.id != id);
        cx.notify();
    }

    /// Recent forwards on these contexts that are not running now, newest first.
    pub fn recent(&self, contexts: &[String]) -> Vec<SavedForward> {
        // Runs on every frame of the sidebar: read the settings in place.
        settings::with(|st| {
            let mut out: Vec<SavedForward> = Vec::new();
            for s in st.recent_forwards.iter().chain(&st.saved_forwards) {
                if contexts.contains(&s.context)
                    && !self.items.iter().any(|f| same(&f.saved, s))
                    && !out.iter().any(|x| same(x, s))
                {
                    out.push(s.clone());
                }
            }
            out
        })
    }

    /// Saved forwards for these contexts that are not running now.
    pub fn restorable(&self, contexts: &[String]) -> Vec<SavedForward> {
        settings::with(|st| {
            st.saved_forwards
                .iter()
                .filter(|s| contexts.contains(&s.context))
                .filter(|s| !self.items.iter().any(|f| same(&f.saved, s)))
                .cloned()
                .collect()
        })
    }
}

/// The same forward, whatever pod currently backs it.
fn same(a: &SavedForward, b: &SavedForward) -> bool {
    a.context == b.context
        && a.namespace == b.namespace
        && a.remote == b.remote
        && match (&a.service, &b.service) {
            (Some(x), Some(y)) => x == y,
            (None, None) => pod_base(&a.pod) == pod_base(&b.pod),
            _ => false,
        }
}

/// "web-7f9d8c-abc12" -> "web" (Deployment pods), "db-0" stays "db-0".
pub fn pod_base(pod: &str) -> &str {
    let parts: Vec<&str> = pod.rsplitn(3, '-').collect();
    // ReplicaSet pods: <deployment>-<6–10 char template hash>-<5 char suffix>.
    let alnum = |s: &str| s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if parts.len() == 3
        && parts[0].len() == 5
        && alnum(parts[0])
        && (6..=10).contains(&parts[1].len())
        && alnum(parts[1])
    {
        parts[2]
    } else {
        pod
    }
}

fn remember(saved: SavedForward) {
    settings::update(|st| {
        st.saved_forwards.retain(|x| !same(x, &saved));
        st.saved_forwards.push(saved.clone());
        st.recent_forwards.retain(|x| !same(x, &saved));
        st.recent_forwards.insert(0, saved);
        st.recent_forwards.truncate(10);
    });
}

/// Forget a forward: no longer offered again or restored.
pub fn forget(saved: &SavedForward) {
    settings::update(|st| {
        st.saved_forwards.retain(|x| !same(x, saved));
        st.recent_forwards.retain(|x| !same(x, saved));
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn pod_bases() {
        assert_eq!(super::pod_base("web-7f9d8c4b5-abc12"), "web");
        assert_eq!(super::pod_base("cargo-staging-spv1-keycloak-0"), "cargo-staging-spv1-keycloak-0");
        assert_eq!(super::pod_base("api"), "api");
    }

    #[test]
    fn brackets_ipv6_hosts() {
        assert_eq!(super::bracketed("::1"), "[::1]");
        assert_eq!(super::bracketed("[::1]"), "[::1]");
        assert_eq!(super::bracketed("192.168.1.5"), "192.168.1.5");
    }
}
