//! Build script: embed the short git HEAD SHA as a compile-time env var so
//! `leancd --version` and the startup log can identify the exact source the
//! binary was built from.
//!
//! Falls back to `unknown` when `git` or a `.git` directory is unavailable
//! (e.g. a tarball source export, or a container build that excludes `.git`).
//! The fallback still yields a well-formed version string (see `version.rs`).

use std::process::Command;

fn main() {
    // An explicit SHA (e.g. passed via `--build-arg GIT_SHA=...` in a container
    // build whose context excludes `.git`) takes precedence over running `git`,
    // so the version string is correct even without a working tree.
    let sha = std::env::var("LEANC_BUILD_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_short_sha)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LEANC_BUILD_GIT_SHA={sha}");
    // Rebuild when the override env var changes or HEAD moves. `.git/HEAD`
    // points at the current branch ref; `.git/refs` covers packed/unpacked
    // ref updates.
    println!("cargo:rerun-if-env-changed=LEANC_BUILD_GIT_SHA");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}

/// Run `git rev-parse --short=8 HEAD` and return the trimmed SHA on success.
fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}
