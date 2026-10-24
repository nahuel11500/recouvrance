//! Local MCP server for AI agents, and the shared state the UI uses to
//! manage it: agents, their access rules, approvals and the activity log.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

mod audit;
mod config;
mod hub;
mod policy;
pub mod rules_file;
pub mod server;

pub use audit::{AuditEntry, Decision};
pub use config::{Agent, config_in_use};
pub use hub::{Answer, Approval, Hub, HubEvent};
pub use policy::{Access, Mode, Policy, Rule, VERBS, any_glob, is_write};

pub const DEFAULT_PORT: u16 = 47070;

// A panic in one call must not lock every agent out: a poisoned lock is still used.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh directory for a test.
#[cfg(test)]
fn test_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
