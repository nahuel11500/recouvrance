//! Where the time goes (`RECOUVRANCE_TIMINGS=1`): named spans, summed per name and printed to
//! stderr every 10 seconds (count, total, average, slowest).

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("RECOUVRANCE_TIMINGS").is_ok());
/// Per span name: count, total, slowest.
type Sums = BTreeMap<&'static str, (u64, Duration, Duration)>;

/// Since when, and the sums so far.
static SPANS: LazyLock<Mutex<(Instant, Sums)>> = LazyLock::new(|| Mutex::new((Instant::now(), BTreeMap::new())));

/// Ends its span when dropped.
pub struct Span(&'static str, Instant);

impl Drop for Span {
    fn drop(&mut self) {
        let took = self.1.elapsed();
        let mut s = SPANS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = s.1.entry(self.0).or_default();
        e.0 += 1;
        e.1 += took;
        e.2 = e.2.max(took);
        if s.0.elapsed() >= Duration::from_secs(10) {
            let mut out = String::from("timings (last 10 s):\n");
            for (name, (n, total, max)) in &s.1 {
                out += &format!(
                    "  {name:<28} {n:>6}× total {:>8.1} ms  avg {:>7.2} ms  max {:>7.1} ms\n",
                    total.as_secs_f64() * 1e3,
                    total.as_secs_f64() * 1e3 / *n as f64,
                    max.as_secs_f64() * 1e3
                );
            }
            eprint!("{out}");
            *s = (Instant::now(), BTreeMap::new());
        }
    }
}

pub fn span(name: &'static str) -> Option<Span> {
    ON.then(|| Span(name, Instant::now()))
}
