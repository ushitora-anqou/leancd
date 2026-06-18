//! Build-time version information.
//!
//! The short git HEAD SHA is embedded at compile time by `build.rs` (see the
//! `LEANC_BUILD_GIT_SHA` environment variable). `FULL_VERSION` composes the
//! crate version with the SHA so `leancd --version` and the startup log can
//! identify the exact source a binary was built from. When `git` is unavailable
//! the SHA is `unknown`, which still yields a well-formed version string.

/// Crate version, taken from `CARGO_PKG_VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git HEAD SHA embedded at build time, or `unknown` when git is absent.
pub const GIT_SHA: &str = env!("LEANC_BUILD_GIT_SHA");

/// `"<version> (sha <sha>)"` for `--version` and the startup log.
///
/// Built with `concat!` so it is a `const &str`; a `format!` would be non-const
/// and allocate. Both the git-available and `unknown` fallback forms are
/// well-formed: `"0.1.0 (sha abc12345)"` / `"0.1.0 (sha unknown)"`.
pub const FULL_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (sha ",
    env!("LEANC_BUILD_GIT_SHA"),
    ")"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_version_starts_with_crate_version() {
        assert!(
            FULL_VERSION.starts_with(VERSION),
            "FULL_VERSION {FULL_VERSION:?} should start with crate version {VERSION:?}"
        );
    }

    #[test]
    fn full_version_contains_sha_marker() {
        // Both the git-available and `unknown` fallback forms include "(sha ...)".
        assert!(
            FULL_VERSION.contains("(sha "),
            "FULL_VERSION {FULL_VERSION:?} should contain '(sha '"
        );
    }

    #[test]
    fn full_version_contains_git_sha() {
        assert!(
            FULL_VERSION.contains(GIT_SHA),
            "FULL_VERSION {FULL_VERSION:?} should contain GIT_SHA {GIT_SHA:?}"
        );
    }

    #[test]
    fn full_version_is_non_empty() {
        assert!(!FULL_VERSION.is_empty());
    }
}
