//! The agents and their settings (agents.json), and the lock that keeps one process using it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::DEFAULT_PORT;
use super::policy::Policy;

/// Whether two lists of agents give the same rules.yaml, which holds each agent's name
/// and rules, not whether it is enabled.
pub(super) fn same_rules(a: &[Agent], b: &[Agent]) -> bool {
    let rules = |agents: &[Agent]| {
        agents.iter().map(|a| Some((a.name.clone(), serde_json::to_value(&a.policy).ok()?))).collect::<Option<Vec<_>>>()
    };
    matches!((rules(a), rules(b)), (Some(x), Some(y)) if x == y)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    /// SHA-256 of the bearer token; the token itself is only shown once.
    pub token_sha256: String,
    pub enabled: bool,
    pub policy: Policy,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    /// Global kill switch: when false every agent call is refused.
    pub enabled: bool,
    pub agents: Vec<Agent>,
    /// 0 (missing) when saved before 0.3.0, which created agents without a section in
    /// rules.yaml: see [`Hub::apply_rules_file`].
    #[serde(default)]
    pub version: u32,
}

/// agents.json's current `version`: rules.yaml names every agent.
pub(super) const CONFIG_VERSION: u32 = 1;

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, enabled: true, agents: Vec::new(), version: CONFIG_VERSION }
    }
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn new_token() -> String {
    format!("kv_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// agents.json; missing means the defaults. One that can't be read (corrupt, or from a
/// newer version) is kept aside so the next save does not destroy its agents and rules,
/// and agent access starts off, with the reason to show.
pub(super) fn load_config(path: &std::path::Path) -> (Config, Option<String>) {
    let read = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
        r => r.map_err(anyhow::Error::from),
    };
    match read.and_then(|bytes| Ok(serde_json::from_slice::<Config>(&bytes)?)) {
        Ok(config) => (config, None),
        Err(e) => {
            let kept = crate::settings::keep_aside(path);
            let error = format!("agents.json could not be read ({e:#}; {kept}): agent access is off");
            (Config { enabled: false, ..Config::default() }, Some(error))
        }
    }
}

pub(super) fn config_lock_path() -> PathBuf {
    crate::paths::config_dir().join("agents.lock")
}

/// Whether the app or `serve` is running: it keeps agents.json in memory and saves it
/// whole, so a change written by another process would be lost.
pub fn config_in_use() -> bool {
    locked(&config_lock_path())
}

/// Whether another open file holds the lock on `path`.
fn locked(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Lock `path` (created if missing) for as long as the returned file is open.
pub(super) fn take_lock(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::test_dir as temp_dir;

    #[test]
    fn a_running_hub_is_seen() {
        let path = temp_dir().join("agents.lock");
        assert!(!locked(&path));
        let held = take_lock(&path).unwrap();
        assert!(locked(&path));
        // A second process (or hub) can't take it while it is held.
        assert!(take_lock(&path).is_none());
        drop(held);
        assert!(!locked(&path));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_bad_agents_file_is_kept_and_turns_agents_off() {
        let dir = temp_dir();
        let path = dir.join("agents.json");
        let (config, error) = load_config(&path);
        assert!(config.enabled && error.is_none());
        std::fs::write(&path, br#"{"enabled": true, "agents": [{"id": 1}]}"#).unwrap();
        let (config, error) = load_config(&path);
        assert!(!config.enabled);
        assert!(config.agents.is_empty());
        assert!(error.is_some());
        // The file is kept aside, so the next save can't destroy it.
        assert!(!path.exists());
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("agents.json.bad-"));
        assert!(kept);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_agents_file_from_before_0_3_0_is_told_apart() {
        let dir = temp_dir();
        let path = dir.join("agents.json");
        // As 0.2.0 saved it: no version.
        std::fs::write(&path, br#"{"port": 7323, "enabled": true, "agents": []}"#).unwrap();
        assert_eq!(load_config(&path).0.version, 0);
        assert_eq!(Config::default().version, CONFIG_VERSION);
        std::fs::write(&path, serde_json::to_vec(&Config::default()).unwrap()).unwrap();
        assert_eq!(load_config(&path).0.version, CONFIG_VERSION);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_hash_is_stable() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        assert_eq!(hash_token("abc").len(), 64);
    }
}
