//! YAML from the editor, apply, plugins and rules files: parsing never panics, and what we
//! write back reads as the same value (the YAML view must round-trip).
#![no_main]

use libfuzzer_sys::fuzz_target;

// The app's own source file, so the target tests exactly what ships.
#[path = "../../src/yaml.rs"]
mod yaml;

fuzz_target!(|text: &str| {
    let _ = yaml::documents(text);
    if let Ok(value) = yaml::from_str::<serde_json::Value>(text) {
        let written = yaml::to_string(&value).expect("a parsed value serializes");
        let back: serde_json::Value = yaml::from_str(&written).expect("what we write parses");
        assert_eq!(back, value, "round trip changed the value:\n{written}");
    }
});
