//! Port-forwards agents opened. They listen on this machine only, are listed in the sidebar,
//! and stop when the agent or the user stops them, when their time is up, when agent access
//! is switched off, or when the agent is disabled or deleted.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::lock;
use crate::k8s::TaskGuard;

/// Longest an agent's forward runs, in minutes.
pub const MAX_MINUTES: u32 = 8 * 60;
/// When the agent doesn't say.
pub const DEFAULT_MINUTES: u32 = 60;

/// What a forward is, for the agent and the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardInfo {
    pub id: u64,
    pub agent_id: String,
    pub agent: String,
    pub context: String,
    pub namespace: String,
    /// `pod/name` or `service/name`.
    pub target: String,
    /// The port on the pod.
    pub remote: u16,
    /// Where it listens, on 127.0.0.1.
    pub local: u16,
    pub until: jiff::Timestamp,
    pub connections: usize,
    pub error: Option<String>,
}

impl ForwardInfo {
    /// `127.0.0.1:8080 → pod/web:80 -n shop --context prod (agent claude, until 14:02)`.
    pub fn describe(&self) -> String {
        let until = self.until.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M");
        format!(
            "127.0.0.1:{} → {}:{} -n {} --context {} (id {}, until {until})",
            self.local, self.target, self.remote, self.namespace, self.context, self.id
        )
    }
}

struct Running {
    info: ForwardInfo,
    /// The listener and its tunnels, and the task following them (expiry included).
    _guards: (TaskGuard, Option<TaskGuard>),
}

#[derive(Default)]
pub struct AgentForwards {
    items: Mutex<Vec<Running>>,
    next: AtomicU64,
}

impl AgentForwards {
    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn add(&self, info: ForwardInfo, forward: TaskGuard) {
        lock(&self.items).push(Running { info, _guards: (forward, None) });
    }

    /// The task that follows forward `id`, kept (and stopped) with it. Dropped at once when
    /// the forward was stopped meanwhile.
    pub fn set_watcher(&self, id: u64, watcher: TaskGuard) {
        if let Some(r) = lock(&self.items).iter_mut().find(|r| r.info.id == id) {
            r._guards.1 = Some(watcher);
        }
    }

    pub fn list(&self) -> Vec<ForwardInfo> {
        lock(&self.items).iter().map(|r| r.info.clone()).collect()
    }

    pub fn update(&self, id: u64, f: impl FnOnce(&mut ForwardInfo)) {
        if let Some(r) = lock(&self.items).iter_mut().find(|r| r.info.id == id) {
            f(&mut r.info);
        }
    }

    /// Stop the forwards `which` picks; returns how many. Their tasks are dropped outside
    /// the lock (a watcher stopping its own forward ends there).
    pub fn stop(&self, which: impl Fn(&ForwardInfo) -> bool) -> usize {
        let stopped: Vec<Running> = {
            let mut items = lock(&self.items);
            let (gone, kept) = std::mem::take(&mut *items).into_iter().partition(|r| which(&r.info));
            *items = kept;
            gone
        };
        stopped.len()
    }
}

/// The port on the pod behind `port` of a Service: its `targetPort`, by number or by the
/// name of a container port of `pod`. A port the Service doesn't have is taken as the pod's.
pub fn service_target_port(service: &serde_json::Value, port: u16, pod: &serde_json::Value) -> Result<u16, String> {
    let ports = service.pointer("/spec/ports").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    let Some(sp) = ports.iter().find(|p| p["port"].as_u64() == Some(u64::from(port))) else {
        return Ok(port);
    };
    match &sp["targetPort"] {
        serde_json::Value::Null => Ok(port),
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or("bad targetPort".into()),
        serde_json::Value::String(name) => pod
            .pointer("/spec/containers")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .flat_map(|c| c["ports"].as_array().cloned().unwrap_or_default())
            .find(|p| p["name"].as_str() == Some(name))
            .and_then(|p| p["containerPort"].as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| format!("no container port named {name:?} on the pod")),
        _ => Err("bad targetPort".into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AgentForwards, ForwardInfo, service_target_port};

    fn info(id: u64, agent: &str) -> ForwardInfo {
        ForwardInfo {
            id,
            agent_id: agent.into(),
            agent: agent.into(),
            context: "kind".into(),
            namespace: "shop".into(),
            target: "pod/web".into(),
            remote: 80,
            local: 8080,
            until: jiff::Timestamp::UNIX_EPOCH,
            connections: 0,
            error: None,
        }
    }

    #[test]
    fn stops_only_what_it_is_asked_to() {
        let forwards = AgentForwards::default();
        for (id, agent) in [(1, "a"), (2, "b"), (3, "a")] {
            forwards.add(info(id, agent), crate::k8s::spawn_guarded(std::future::pending()));
        }
        assert_eq!(forwards.stop(|f| f.id == 2), 1);
        assert_eq!(forwards.stop(|f| f.agent_id == "a"), 2);
        assert!(forwards.list().is_empty());
    }

    #[test]
    fn a_service_port_leads_to_its_target_port() {
        let svc = json!({"spec": {"ports": [
            {"port": 80, "targetPort": 8080},
            {"port": 443, "targetPort": "https"},
            {"port": 9000},
        ]}});
        let pod = json!({"spec": {"containers": [{"ports": [{"name": "https", "containerPort": 8443}]}]}});
        assert_eq!(service_target_port(&svc, 80, &pod), Ok(8080));
        assert_eq!(service_target_port(&svc, 443, &pod), Ok(8443));
        assert_eq!(service_target_port(&svc, 9000, &pod), Ok(9000));
        assert_eq!(service_target_port(&svc, 5432, &pod), Ok(5432), "not a Service port: the pod's");
        assert!(service_target_port(&svc, 443, &json!({})).is_err());
    }
}
