//! Boundary test: Director's core must not know its substrates exist.
//!
//! This is the enforcement of the integration rule from Phase 0:
//!
//! > Director talks to both substrates exclusively over MCP, never by importing
//! > their internal storage structs.
//!
//! A developer can intend that and still break it — one `use handoff_mcp::...`
//! in the wrong crate and the boundary is gone. So the rule is checked by a
//! test, not by good intentions.
//!
//! ## What counts as coupling
//!
//! - A `.rs` file outside `director-adapters` containing the crate identifiers
//!   `handoff_mcp` or `ai_memory`.
//! - A `Cargo.toml` outside `director-adapters` declaring a dependency whose
//!   name contains `handoff` or `ai-memory`.
//!
//! ## What does not count
//!
//! Prose. `director-domain` documents *why* it differs from the substrates, and
//! those doc comments name them. That is the opposite of coupling: it is the
//! design rationale for avoiding it. The test therefore looks for the
//! identifiers a `use` statement would need, not for English.

#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};

/// The one crate permitted to name a substrate.
const ADAPTER_CRATE_DIR: &str = "director-adapters";

/// Directories that are not source and must be skipped.
const SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// Rust identifiers by which a substrate crate would be referenced in code.
const SUBSTRATE_IDENTIFIERS: [&str; 2] = ["handoff_mcp", "ai_memory"];

/// Substrings that mark a Cargo dependency as a substrate.
const SUBSTRATE_DEP_NAMES: [&str; 2] = ["handoff", "ai-memory"];

fn workspace_root() -> PathBuf {
    // crates/director-adapters -> workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/<crate>")
        .to_path_buf()
}

/// Recursively collect files under `root` matching `suffix`, skipping build
/// artifacts and the adapter crate.
fn collect_files(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if path.is_dir() {
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            // The adapter crate is allowed to know about substrates.
            if name == ADAPTER_CRATE_DIR {
                continue;
            }
            collect_files(&path, suffix, out);
        } else if name.ends_with(suffix) {
            out.push(path);
        }
    }
}

#[test]
fn no_rust_file_outside_the_adapters_references_a_substrate() {
    let root = workspace_root();
    let mut rust_files = Vec::new();
    collect_files(&root, ".rs", &mut rust_files);

    assert!(
        !rust_files.is_empty(),
        "found no .rs files to check; the test is broken"
    );

    let mut offenders = Vec::new();
    for file in &rust_files {
        let Ok(contents) = fs::read_to_string(file) else {
            continue;
        };
        for identifier in SUBSTRATE_IDENTIFIERS {
            if contents.contains(identifier) {
                offenders.push(format!("{}: references `{identifier}`", file.display()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "substrate coupling found outside {ADAPTER_CRATE_DIR}:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_cargo_manifest_outside_the_adapters_depends_on_a_substrate() {
    let root = workspace_root();
    let mut manifests = Vec::new();
    collect_files(&root, "Cargo.toml", &mut manifests);

    let mut offenders = Vec::new();
    for file in &manifests {
        let Ok(contents) = fs::read_to_string(file) else {
            continue;
        };

        // Walk the manifest line by line, tracking whether we are inside a
        // dependency table. This is deliberately a small parser rather than a
        // full TOML dependency, because a false positive here would train
        // people to ignore the test.
        let mut in_dependency_table = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependency_table = trimmed.contains("dependencies");
                continue;
            }
            if !in_dependency_table {
                continue;
            }
            // A dependency key is the text before `=` or a bare name.
            let key = trimmed.split(['=', ' ']).next().unwrap_or("");
            let key = key.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
            if SUBSTRATE_DEP_NAMES
                .iter()
                .any(|substrate| key.contains(substrate))
            {
                offenders.push(format!("{}: depends on `{key}`", file.display()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "substrate dependency found outside {ADAPTER_CRATE_DIR}:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_workspace_contains_the_expected_crates() {
    // Guards against the boundary test silently checking an empty tree.
    let root = workspace_root();
    assert!(root.join("crates/director-domain/src/lib.rs").exists());
    assert!(root.join("crates/director-adapters/src/lib.rs").exists());
}
