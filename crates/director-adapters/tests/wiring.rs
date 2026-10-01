//! Module wiring test: every source file is compiled.
//!
//! This test exists because of a real bug in this repo. The PLAN step was
//! written to `crates/director-app/src/plan.rs` — 619 lines, with tests — and
//! then never declared as a module in the crate root. The file sat in the tree
//! for days: never compiled, never linted, its ten tests never run, while the
//! README said PLAN was "built next". Nothing in the toolchain complained. An
//! undeclared file is invisible to `cargo`, and the only thing that noticed was
//! an audit.
//!
//! A developer can intend "wire this in before I go further" and still forget.
//! So the rule is checked by a test, not by good intentions — the same
//! reasoning behind this crate's boundary test, which keeps the substrate
//! boundary enforced rather than aspirational. Both tests turn an architectural
//! intention into something that fails the build.
//!
//! ## What counts as wired
//!
//! - A file `foo.rs` under `src/` — other than the crate root itself — is
//!   declared as `mod foo;` in the module that owns it: the crate root for
//!   top-level files, or the directory's `mod.rs` (or the same-named file
//!   beside the directory) for nested ones.
//!
//! The reverse need not be checked here. A `mod foo;` with no `foo.rs` behind
//! it is already a hard compile error, so the compiler catches that direction
//! unaided. What no part of the toolchain catches is a file that *exists* and
//! is never declared: that compiles fine, it just compiles less than the author
//! intended — which is precisely how the bug above went unnoticed.
//!
//! ## What does not count
//!
//! `mod foo { ... }` with a body. That is an inline module: it declares no
//! file, and it is not what makes a `foo.rs` compile. The parser deliberately
//! ignores it, so the inline `mod tests` blocks throughout this codebase do not
//! read as declarations.

#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};

/// The names a Rust module file can have. Everything else under `src/` must be
/// declared by one of these.
const MODULE_FILE_NAMES: [&str; 3] = ["lib.rs", "main.rs", "mod.rs"];

/// crates/director-adapters -> workspace root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/<crate>")
        .to_path_buf()
}

/// Whether `path` is a module file itself — the crate root or a `mod.rs` —
/// rather than a module that one of those declares.
fn is_module_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| MODULE_FILE_NAMES.contains(&name))
}

/// The file that declares the modules inside `dir`, if `dir` is reachable from
/// the crate root at all.
///
/// A directory's children are declared in its own `mod.rs`, in a same-named
/// file beside it (`src/foo.rs` declaring `src/foo/`), or — at the crate root —
/// in `lib.rs` or `main.rs`.
fn module_file_for(dir: &Path) -> Option<PathBuf> {
    for name in MODULE_FILE_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // The mod-less layout: `src/foo.rs` declares the `src/foo/` directory.
    let parent = dir.parent()?;
    let name = dir.file_name()?.to_str()?;
    let candidate = parent.join(format!("{name}.rs"));
    candidate.is_file().then_some(candidate)
}

/// The external modules declared in a module file: the identifiers in
/// `mod foo;`, at any visibility.
///
/// Only the semicolon form counts. `mod foo {` is an inline module: it has no
/// file behind it, and it does not make a `foo.rs` compile — so counting it
/// would let an undeclared file pass whenever some unrelated inline module
/// happened to share its name. That is not hypothetical: this codebase has an
/// inline `mod tests` in most of its files.
fn declared_modules(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in source.lines() {
        // Comments can discuss modules; they must not be parsed as declarations.
        let line = line.trim_start();
        if line.starts_with("//") {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(position) = tokens.iter().position(|token| *token == "mod") else {
            continue;
        };
        // `pub mod foo;`, `pub(crate) mod foo;` and `#[attr] mod foo;` all put
        // the keyword somewhere after position 0, so locate it rather than
        // assuming it comes first.
        let Some(name) = tokens.get(position + 1) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(';') {
            names.push(stem.to_string());
        }
    }
    names
}

/// Check one directory: every source file in it is declared by its module file.
/// Problems accumulate instead of short-circuiting, so a single run reports
/// every gap at once.
fn check_dir(dir: &Path, problems: &mut Vec<String>, files_checked: &mut usize) {
    // A directory with no module file is unreachable: nothing declares it, so
    // every file inside is dead code regardless of what else is true.
    let Some(declarer) = module_file_for(dir) else {
        for entry in fs::read_dir(dir).expect("a source directory is readable") {
            let path = entry.expect("a directory entry is valid").path();
            if path.is_dir() {
                check_dir(&path, problems, files_checked);
            } else if is_rust(&path) && !is_module_file(&path) {
                *files_checked += 1;
                problems.push(format!(
                    "{}: no module file declares this directory, so this file is not compiled",
                    path.display()
                ));
            }
        }
        return;
    };

    let source = fs::read_to_string(&declarer)
        .unwrap_or_else(|_| panic!("cannot read {}", declarer.display()));
    let declared = declared_modules(&source);

    let mut subdirectories = Vec::new();
    for entry in fs::read_dir(dir).expect("a source directory is readable") {
        let path = entry.expect("a directory entry is valid").path();
        if path.is_dir() {
            subdirectories.push(path);
            continue;
        }
        if !is_rust(&path) || is_module_file(&path) {
            continue;
        }
        *files_checked += 1;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("an .rs file has a utf-8 stem");
        if !declared.iter().any(|name| name == stem) {
            problems.push(format!(
                "{}: not declared as a module in {} — add `mod {stem};`",
                path.display(),
                declarer.display()
            ));
        }
    }

    // Note that only the forward direction needs checking here. The inverse —
    // a `mod foo;` with no file behind it — is already a compile error, so the
    // compiler catches it unaided. What no part of the toolchain catches is a
    // file that exists and is never declared: that compiles fine, it just
    // compiles less than the author intended, which is the bug this test is for.

    for subdirectory in subdirectories {
        check_dir(&subdirectory, problems, files_checked);
    }
}

fn is_rust(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("rs")
}

#[test]
fn every_source_file_is_reachable_from_the_module_tree() {
    let crates_dir = workspace_root().join("crates");
    assert!(crates_dir.is_dir(), "the workspace has a crates/ directory");

    let mut problems = Vec::new();
    let mut files_checked = 0usize;
    let mut crates_checked = Vec::new();
    for crate_dir in fs::read_dir(&crates_dir).expect("crates/ is readable") {
        let crate_dir = crate_dir.expect("a directory entry is valid").path();
        let src = crate_dir.join("src");
        if !src.is_dir() {
            continue;
        }
        if let Some(name) = crate_dir.file_name().and_then(|name| name.to_str()) {
            crates_checked.push(name.to_string());
        }
        check_dir(&src, &mut problems, &mut files_checked);
    }

    // Guards against the test silently passing on an empty tree.
    assert!(
        files_checked > 0,
        "the walk checked no files; the test is broken"
    );
    for expected in ["director-domain", "director-app"] {
        assert!(
            crates_checked.iter().any(|name| name == expected),
            "the walk did not reach {expected}; found {:?}",
            crates_checked
        );
    }

    assert!(
        problems.is_empty(),
        "unreachable or dangling modules found:\n{}",
        problems.join("\n")
    );
}
