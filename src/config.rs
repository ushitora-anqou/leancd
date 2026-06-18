//! Runtime configuration for leancd.
//!
//! All configuration is provided via command-line flags, and only secrets (git
//! credentials) are read from the environment, which in Kubernetes are injected
//! from a Secret. A single process maps to a single Git repository (the
//! configured path).

use crate::error::{Error, Result};

/// Static configuration derived from CLI flags.
///
/// `Default` exists only to make merging with overrides convenient in tests;
/// production values always come from parsed CLI arguments.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Git repository URL to sync from (HTTPS or SSH).
    pub repo_url: String,
    /// Branch / ref to track, e.g. `main`.
    pub branch: String,
    /// Glob patterns of directories inside the repository to sync. Each matched
    /// directory is scanned recursively; patterns are OR-combined and deduped.
    /// An empty slice means "the whole repo" (`.`).
    pub path: Vec<String>,
    /// Reconciliation polling interval.
    pub poll_interval: std::time::Duration,
    /// Namespace leancd runs in (used for the state ConfigMap and as the
    /// default namespace for namespaced resources that omit one).
    pub namespace: String,
    /// Name of the ConfigMap leancd writes its sync state into.
    pub state_configmap: String,
    /// Local working directory that holds the shallow checkout.
    pub work_dir: String,

    // --- Secret-derived git credentials (read from the environment) ---
    /// HTTPS basic-auth username. Read from this environment variable if set.
    pub git_username_env: String,
    /// HTTPS basic-auth password / token. Read from this environment variable.
    pub git_password_env: String,
    /// SSH private key (PEM). Read from this environment variable when the
    /// repository is reached over SSH.
    pub git_ssh_key_env: String,

    // --- Apply metadata ---
    /// Label key marking resources managed by leancd (for safe pruning).
    pub managed_label_key: String,
    /// Label value for the managed-by label.
    pub managed_label_value: String,
    /// Server-side-apply field manager identity.
    pub field_manager: String,

    /// Maximum time to wait for a Helm hook (Job/Pod) to reach a terminal
    /// state before treating it as failed.
    pub hook_timeout: std::time::Duration,

    /// Base delay for exponential backoff after a failed reconciliation pass.
    /// The first retry waits `backoff_base`, then `2*backoff_base`, and so on,
    /// capped at `backoff_max`, and jittered to `[0.75x, 1.0x)` in the
    /// controller loop (see `reconcile::jittered`). A successful pass resets
    /// the backoff to zero.
    pub backoff_base: std::time::Duration,
    /// Maximum (cap) backoff delay between retries after repeated failures.
    pub backoff_max: std::time::Duration,
    /// Grace period to let an in-flight reconciliation pass finish on shutdown
    /// before force-aborting the task.
    pub shutdown_timeout: std::time::Duration,

    /// A sync is considered stale for health checks when the last successful
    /// sync is older than `poll_interval * health_stale_factor`.
    pub health_stale_factor: u32,
}

impl Config {
    /// Resolve git credentials from the environment, returning HTTPS basic
    /// auth credentials when both username and password are present.
    pub fn git_credentials(&self) -> Option<(String, String)> {
        let user = std::env::var(&self.git_username_env).ok();
        let pass = std::env::var(&self.git_password_env).ok();
        match (user, pass) {
            (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => Some((u, p)),
            _ => None,
        }
    }

    /// Classify the repository URL's transport so callers can pick the matching
    /// authentication path (HTTPS basic auth vs. an injected SSH key).
    pub fn repo_kind(&self) -> RepoKind {
        let url = self.repo_url.trim();
        if url.starts_with("git@") || url.starts_with("ssh://") {
            RepoKind::Ssh
        } else if url.starts_with("https://") || url.starts_with("http://") {
            RepoKind::Https
        } else if url.starts_with("file://") || url.starts_with('/') || url.starts_with('.') {
            RepoKind::File
        } else {
            RepoKind::Other
        }
    }

    /// Read a PEM-encoded SSH private key from the environment, if injected.
    /// Whitespace-only values are treated as absent.
    pub fn ssh_private_key(&self) -> Option<String> {
        let key = std::env::var(&self.git_ssh_key_env).ok()?;
        let key = key.trim();
        if key.is_empty() {
            None
        } else {
            Some(key.to_string())
        }
    }

    /// Construct a URL the `git` CLI can fetch. HTTPS URLs get basic-auth
    /// credentials embedded when both username and password are present; SSH and
    /// every other transport is passed through unchanged (an SSH key, when
    /// configured, is injected separately via `GIT_SSH_COMMAND` by `git_sync`).
    /// The value is never logged.
    pub fn authed_url(&self) -> Result<String> {
        if self.repo_kind() == RepoKind::Https {
            if let Some((user, pass)) = self.git_credentials() {
                return embed_basic_auth(&self.repo_url, &user, &pass);
            }
        }
        Ok(self.repo_url.clone())
    }
}

/// Transport inferred from a Git repository URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    /// `git@host:path` or `ssh://...`.
    Ssh,
    /// `https://` or `http://`.
    Https,
    /// `file://`, or a local path.
    File,
    /// Anything else; passed through verbatim.
    Other,
}

/// Embed `user:pass` into `url` right after its scheme, percent-encoding the
/// userinfo so special characters do not break parsing.
fn embed_basic_auth(url: &str, user: &str, pass: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::Config(format!("repo URL has no scheme separator: {url}")))?;
    Ok(format!(
        "{}://{}:{}@{}",
        scheme,
        urlenc(user),
        urlenc(pass),
        rest
    ))
}

fn urlenc(s: &str) -> String {
    fn hex(b: u8) -> char {
        char::from_digit(b as u32 >> 4, 16).unwrap()
    }
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(hex(b));
                out.push(hex(b << 4));
            }
        }
    }
    out
}

/// Sensible parser for durations like `30s`, `5m`, `2h`.
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("ms") {
        return Ok(std::time::Duration::from_millis(num.parse().map_err(
            |e: std::num::ParseIntError| Error::Config(format!("invalid duration: {e}")),
        )?));
    }
    let (num, mult) = if let Some(n) = s.strip_suffix("s") {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix("m") {
        (n, 60)
    } else if let Some(n) = s.strip_suffix("h") {
        (n, 3600)
    } else {
        return Err(Error::Config(format!(
            "invalid duration '{s}': expected a number followed by s/m/h/ms"
        )));
    };
    let n: u64 = num
        .parse()
        .map_err(|e: std::num::ParseIntError| Error::Config(format!("invalid duration: {e}")))?;
    Ok(std::time::Duration::from_secs(n * mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_seconds() {
        assert_eq!(
            parse_duration("30s").unwrap(),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn duration_minutes_and_hours() {
        assert_eq!(
            parse_duration("5m").unwrap(),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("2h").unwrap(),
            std::time::Duration::from_secs(7200)
        );
    }

    #[test]
    fn duration_millis() {
        assert_eq!(
            parse_duration("500ms").unwrap(),
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn duration_invalid() {
        assert!(parse_duration("soon").is_err());
    }

    /// Minimal `Config` for testing URL/credential helpers.
    fn test_config(repo_url: &str) -> Config {
        Config {
            repo_url: repo_url.to_string(),
            branch: "main".into(),
            path: vec![".".to_string()],
            poll_interval: std::time::Duration::from_secs(60),
            namespace: "default".into(),
            state_configmap: "leancd-state".into(),
            work_dir: "/tmp/leancd-work".into(),
            git_username_env: "GIT_USERNAME".into(),
            git_password_env: "GIT_PASSWORD".into(),
            git_ssh_key_env: "GIT_SSH_KEY".into(),
            managed_label_key: "app.kubernetes.io/managed-by".into(),
            managed_label_value: "leancd".into(),
            field_manager: "leancd".into(),
            hook_timeout: std::time::Duration::from_secs(300),
            backoff_base: std::time::Duration::from_secs(5),
            backoff_max: std::time::Duration::from_secs(600),
            shutdown_timeout: std::time::Duration::from_secs(28),
            health_stale_factor: 10,
        }
    }

    #[test]
    fn repo_kind_classification() {
        let mk = |u: &str| test_config(u).repo_kind();
        assert_eq!(mk("git@github.com:org/repo.git"), RepoKind::Ssh);
        assert_eq!(mk("ssh://git@github.com/org/repo.git"), RepoKind::Ssh);
        assert_eq!(mk("https://github.com/org/repo.git"), RepoKind::Https);
        assert_eq!(mk("http://example.org/r"), RepoKind::Https);
        assert_eq!(mk("file:///srv/repo"), RepoKind::File);
        assert_eq!(mk("/srv/repo"), RepoKind::File);
        assert_eq!(mk("./relative/repo"), RepoKind::File);
        assert_eq!(mk("host:repo"), RepoKind::Other);
    }

    #[test]
    fn embed_basic_auth_into_https() {
        let s = embed_basic_auth("https://github.com/o/r", "alice", "s3cr3t").unwrap();
        assert_eq!(s, "https://alice:s3cr3t@github.com/o/r");
    }

    #[test]
    fn embed_basic_auth_percent_encodes_special() {
        // '@' and '/' in credentials must be encoded so they do not break parsing.
        let s = embed_basic_auth("https://host/path", "a/b", "p@ss").unwrap();
        assert_eq!(s, "https://a%2fb:p%40ss@host/path");
    }

    #[test]
    fn embed_basic_auth_missing_scheme_errors() {
        assert!(embed_basic_auth("host/path", "u", "p").is_err());
    }

    #[test]
    fn authed_url_passes_through_ssh_and_file() {
        assert_eq!(
            test_config("git@github.com:o/r.git").authed_url().unwrap(),
            "git@github.com:o/r.git"
        );
        assert_eq!(
            test_config("file:///srv/repo").authed_url().unwrap(),
            "file:///srv/repo"
        );
    }

    #[test]
    fn authed_url_https_without_creds_passes_through() {
        assert_eq!(
            test_config("https://github.com/o/r").authed_url().unwrap(),
            "https://github.com/o/r"
        );
    }

    #[test]
    fn authed_url_embeds_https_basic_auth() {
        // Unique env var names avoid races with concurrent tests.
        std::env::set_var("LEANCD_TEST_AUTH_USER", "alice");
        std::env::set_var("LEANCD_TEST_AUTH_PASS", "s3cr3t");
        let mut cfg = test_config("https://github.com/o/r");
        cfg.git_username_env = "LEANCD_TEST_AUTH_USER".into();
        cfg.git_password_env = "LEANCD_TEST_AUTH_PASS".into();
        let url = cfg.authed_url().unwrap();
        std::env::remove_var("LEANCD_TEST_AUTH_USER");
        std::env::remove_var("LEANCD_TEST_AUTH_PASS");
        assert_eq!(url, "https://alice:s3cr3t@github.com/o/r");
    }
}
