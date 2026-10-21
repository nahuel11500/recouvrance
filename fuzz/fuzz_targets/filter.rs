//! What you type in a table filter: parsing it, then comparing column terms against values,
//! must never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

// The app's own source files, so the targets test exactly what ships.
#[path = "../../src/glob.rs"]
mod glob;
#[path = "../../src/quantity.rs"]
mod quantity;
#[path = "../../src/filter_syntax.rs"]
mod filter_syntax;
use filter_syntax::{Term, compare, contains_ci, parse_terms};

fuzz_target!(|input: &str| {
    // The input is both the filter and the cell text it compares against.
    let (filter, cell) = input.split_once('\n').unwrap_or((input, ""));
    for (_, term) in parse_terms(filter) {
        match term {
            Term::Column(name, op, want, raw) => {
                compare(&name, cell, op, &want);
                contains_ci(cell, &raw);
            }
            Term::Text(t) => {
                contains_ci(cell, &t);
            }
            Term::Regex(re) => {
                re.is_match(cell);
            }
            _ => {}
        }
    }
});
