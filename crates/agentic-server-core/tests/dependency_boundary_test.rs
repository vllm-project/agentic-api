//! `agentic-server-core` is instrumented with `tracing` only. Providers,
//! exporters, and the subscriber belong to the binary (or the embedding
//! application), so the library's normal dependencies must never include
//! them — `ARCHITECTURE.md` and the phase 2 design on #279 both rely on it.

use std::collections::BTreeSet;

/// Crates that would pull SDK, export, or subscriber machinery into the
/// library. `opentelemetry` (the API crate) and `tracing-opentelemetry`
/// (the bridge) are allowed once phase 2 needs context propagation.
const FORBIDDEN_LIBRARY_DEPENDENCIES: &[&str] = &[
    "opentelemetry_sdk",
    "opentelemetry-sdk",
    "opentelemetry-otlp",
    "opentelemetry-stdout",
    "opentelemetry-prometheus",
    "tracing-subscriber",
];

fn library_dependencies() -> BTreeSet<String> {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&manifest).unwrap();
    let mut names = BTreeSet::new();
    for table in ["dependencies", "build-dependencies"] {
        if let Some(deps) = manifest.get(table).and_then(toml::Value::as_table) {
            names.extend(deps.keys().cloned());
        }
    }
    // Feature-activated optional dependencies count too.
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            if let Some(deps) = target.get("dependencies").and_then(toml::Value::as_table) {
                names.extend(deps.keys().cloned());
            }
        }
    }
    names
}

#[test]
fn core_library_never_depends_on_sdk_exporters_or_subscribers() {
    let dependencies = library_dependencies();
    assert!(
        dependencies.contains("tracing"),
        "instrumentation goes through `tracing`"
    );
    for forbidden in FORBIDDEN_LIBRARY_DEPENDENCIES {
        assert!(
            !dependencies.contains(*forbidden),
            "`{forbidden}` must stay a dev-dependency or live in agentic-server"
        );
    }
}
