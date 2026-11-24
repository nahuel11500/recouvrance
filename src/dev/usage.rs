//! What a dev run cost, printed at its end: each cluster's API requests and bytes by kind of
//! request (as Settings → API requests counts them), the heaviest requests, and the process's
//! peak memory.

use crate::k8s::meter;

/// Print what the run cost so far.
pub fn print() {
    let snapshot = meter::snapshot(meter::now_minute());
    for (context, class, stats) in &snapshot.classes {
        println!(
            "api {context} {}: {} requests, {} failed, {}",
            class.label(),
            stats.count,
            stats.errors,
            megabytes(stats.bytes)
        );
    }
    for heavy in &snapshot.heaviest {
        println!("heaviest {} {} {}: {}", heavy.context, heavy.method, heavy.path, megabytes(heavy.bytes));
    }
    if let Some(peak) = peak_memory() {
        println!("peak memory (VmHWM): {peak}");
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// The process's peak resident memory as Linux counts it; None elsewhere.
fn peak_memory() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|l| l.strip_prefix("VmHWM:")).map(|v| v.trim().to_string())
}
