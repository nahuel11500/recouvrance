//! Kubernetes quantities ("250m", "1.5Gi") read from the API: parsing never panics.
#![no_main]

use libfuzzer_sys::fuzz_target;

// The app's own source file, so the target tests exactly what ships.
#[path = "../../src/quantity.rs"]
mod quantity;
use quantity::{parse_bytes, parse_cpu};

fuzz_target!(|q: &str| {
    let _ = parse_cpu(q);
    let _ = parse_bytes(q);
});
