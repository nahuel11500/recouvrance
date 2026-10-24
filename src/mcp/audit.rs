//! The activity log: every agent call, in memory and in audit.jsonl.

use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(super) const MAX_AUDIT_IN_MEMORY: usize = 2000;
/// audit.jsonl moves to audit.jsonl.1 (replacing the older one) past this size.
const MAX_AUDIT_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allowed,
    Approved,
    Denied,
    Rejected,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditEntry {
    pub time: jiff::Timestamp,
    pub agent_id: String,
    pub agent: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub decision: Decision,
    pub detail: String,
    pub duration_ms: u64,
    pub result_bytes: usize,
    /// MCP session (one agent conversation), when the client sent one.
    #[serde(default)]
    pub session: Option<String>,
}

/// See [`Hub::audit_stats`](super::Hub::audit_stats).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuditStats {
    pub matching: usize,
    pub oldest: Option<jiff::Timestamp>,
    pub newest: Option<jiff::Timestamp>,
    pub full: bool,
}

pub(super) fn audit_path() -> PathBuf {
    crate::paths::data_dir().join("audit.jsonl")
}

/// Append one line, first moving a full log to `<name>.1` (replacing the older one).
pub(super) fn append_audit(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        crate::private_files::create_dir(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= MAX_AUDIT_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(path, old)?;
    }
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
    writeln!(f, "{line}")
}

/// The last entries of the log, reading only its end.
/// All the entries of one log file, skipping lines that don't parse.
pub(super) fn read_audit(path: &std::path::Path) -> Vec<AuditEntry> {
    let text = std::fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&text).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

pub(super) fn load_audit_tail(path: &std::path::Path) -> VecDeque<AuditEntry> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    // Far more than MAX_AUDIT_IN_MEMORY entries of a few hundred bytes.
    const TAIL_BYTES: u64 = 4 * 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return VecDeque::new();
    };
    let start = f.metadata().map_or(0, |m| m.len()).saturating_sub(TAIL_BYTES);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return VecDeque::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line was cut by the seek.
        lines.remove(0);
    }
    let first = lines.len().saturating_sub(MAX_AUDIT_IN_MEMORY);
    lines[first..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::test_dir as temp_dir;

    fn entry(tool: &str) -> AuditEntry {
        AuditEntry {
            time: jiff::Timestamp::now(),
            agent_id: "a".into(),
            agent: "a".into(),
            tool: tool.into(),
            args: serde_json::json!({}),
            decision: Decision::Allowed,
            detail: String::new(),
            duration_ms: 0,
            result_bytes: 0,
            session: None,
        }
    }

    #[test]
    fn audit_log_rotates_and_reads_its_tail() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir();
        let path = dir.join("logs").join("audit.jsonl");
        for i in 0..3 {
            append_audit(&path, &serde_json::to_string(&entry(&format!("t{i}"))).unwrap()).unwrap();
        }
        let tail = load_audit_tail(&path);
        assert_eq!(tail.iter().map(|e| e.tool.as_str()).collect::<Vec<_>>(), ["t0", "t1", "t2"]);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.join("logs")), 0o700);
        // A full log moves aside and a new one starts.
        let f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.set_len(MAX_AUDIT_BYTES).unwrap();
        append_audit(&path, &serde_json::to_string(&entry("new")).unwrap()).unwrap();
        assert_eq!(load_audit_tail(&path).len(), 1);
        assert_eq!(std::fs::metadata(dir.join("logs/audit.jsonl.1")).unwrap().len(), MAX_AUDIT_BYTES);
        // Only the end of a big file is read; the line cut by the seek is skipped.
        let big = dir.join("big.jsonl");
        let line = serde_json::to_string(&entry("x")).unwrap();
        let text = format!("{line}\n").repeat(5 * 1024 * 1024 / line.len());
        std::fs::write(&big, text).unwrap();
        let tail = load_audit_tail(&big);
        assert_eq!(tail.len(), MAX_AUDIT_IN_MEMORY);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
