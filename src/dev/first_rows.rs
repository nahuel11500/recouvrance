//! How long a connect takes to show rows (`RECOUVRANCE_FIRSTROWS=1`): from the moment the
//! contexts are picked (or reconnected at launch), it prints to stderr when each cluster's
//! first rows reach the table and when its list is complete. With several contexts, one of
//! them slow or unreachable, it shows whether the others wait for it.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("RECOUVRANCE_FIRSTROWS").is_ok());

/// When the connect started, and the clusters already reported.
struct State {
    start: Instant,
    rows: HashSet<String>,
    listed: HashSet<String>,
}

impl State {
    fn new() -> Self {
        Self { start: Instant::now(), rows: HashSet::new(), listed: HashSet::new() }
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::new()));

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A connect starts: times count from here.
pub fn connecting(contexts: &[String]) {
    if !*ON {
        return;
    }
    *state() = State::new();
    eprintln!("first-rows: connecting to {}", contexts.join(", "));
}

/// The list's table took a message of `context`'s watch: its first rows, or its list complete.
pub fn seen(context: &str, rows: bool, listed: bool) {
    if !*ON {
        return;
    }
    let mut s = state();
    let at = s.start.elapsed();
    if rows && s.rows.insert(context.to_string()) {
        eprintln!("first-rows: {context} rows at {:.2}s", at.as_secs_f64());
    }
    if listed && s.listed.insert(context.to_string()) {
        eprintln!("first-rows: {context} listed at {:.2}s", at.as_secs_f64());
    }
}
