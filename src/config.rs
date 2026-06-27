//! Runtime configuration for Lean CD.
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
    /// Namespace Lean CD runs in (used for the state ConfigMap and as the
    /// default namespace for namespaced resources that omit one).
    pub namespace: String,
    /// Name of the ConfigMap Lean CD writes its sync state into.
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
    /// Label key marking resources managed by Lean CD (for safe pruning).
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

    /// Lifetime of the reconcile-exclusion Lease (see `lock.rs`). A holder that
    /// stops renewing for this long is treated as stale, and another process may
    /// forcibly acquire the lease. Must exceed the longest normal reconcile pass
    /// (including hook waits); a crashed holder's lease is reclaimed after this
    /// duration. Correctness (no concurrent passes) takes priority over the
    /// memory budget — this mechanism exists precisely to guarantee it.
    pub lock_lease_duration: std::time::Duration,

    /// How long to wait for the reconcile Lease when another pass already holds
    /// it, before giving up with a "busy" skip (an INFO log, not an error). Long
    /// enough to ride out a peer's quick pass, short enough that a manual `sync`
    /// does not block excessively.
    pub lock_wait_timeout: std::time::Duration,

    /// How cluster-side resource changes wake the reconcile loop
    /// (`off`/`trigger`/`cache`). See `watch.rs`. `off` (default) means
    /// periodic-poll drift detection only — today's behavior.
    pub watch_mode: crate::watch::WatchMode,
    /// Debounce window for watch-triggered reconciles: a burst of watch events
    /// within this window collapses into one pass (avoids N passes on a
    /// reconnect `InitApply` burst or a rapid edit storm).
    pub watch_debounce: std::time::Duration,

    /// In `cache` watch mode, the maximum serialized size (bytes) of an object
    /// to cache in full. Objects larger than this are tracked by key only and
    /// drift-checked via a per-GVK `List` fallback, so the cache's RSS does not
    /// grow with per-object payload size. `0` means everything is LargeTier
    /// (no bodies cached; drift is fully List-based). Size-based, not kind-based
    /// — any resource kind can carry a large `data`/`spec`.
    pub cache_max_object_bytes: usize,

    /// Whether to evaluate and publish Argo CD-style resource health (the worst
    /// status across managed resources with a built-in health check). Defaults
    /// on; disable to skip the health-assessment pass and the metric. Sync
    /// completion is unaffected either way (apply success still completes a
    /// sync) — health is an independent signal.
    pub health_enabled: bool,

    /// When true, `reconcile` validates the desired manifests via a server-side
    /// **dry-run** apply and reports what would change, without mutating the
    /// cluster or persisting state. Hooks and pruning are skipped (no side
    /// effects), and `last_sha`/state are not advanced. Set by `sync --dry-run`.
    pub dry_run: bool,
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

    /// PID-scoped path of the actual working directory (`{work_dir}-{pid}`).
    ///
    /// The controller process and a concurrent `leancd sync` (e.g. via
    /// `kubectl exec` in the same Pod) each get their own shallow checkout, so
    /// `git fetch`/`reset`/`clone` never touch the same directory at once. A
    /// separate Pod already has its own emptyDir, so the PID suffix is harmless
    /// there. The whole reconcile pass is additionally serialized by the
    /// `lock.rs` Lease — PID-scoping is the second layer of defense.
    pub fn effective_work_dir(&self) -> String {
        format!(
            "{}-{}",
            self.work_dir.trim_end_matches('/'),
            std::process::id()
        )
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

/// Embed `user:pass` into `url` as HTTP basic-auth userinfo. `url::Url`
/// percent-encodes the credentials and may normalize the URL (lowercase host,
/// drop default port) — accepted as the trade-off for using the `url` crate.
/// SSH/SCP URLs (which `url::Url` cannot parse) never reach here: `authed_url`
/// only calls this for HTTPS URLs.
fn embed_basic_auth(url: &str, user: &str, pass: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| Error::Config(format!("invalid repo URL '{url}': {e}")))?;
    parsed
        .set_username(user)
        .map_err(|_| Error::Config("invalid git username".into()))?;
    parsed
        .set_password(Some(pass))
        .map_err(|_| Error::Config("invalid git password".into()))?;
    Ok(parsed.to_string())
}

/// Parse a human-friendly duration: `30s`, `5m`, `2h`, `500ms`, and compound
/// forms like `1h30m` / `1h 30m` / `30sec`. Wraps `humantime::parse_duration`;
/// maps its error into [`Error::Config`].
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    humantime::parse_duration(s).map_err(|e| Error::Config(format!("invalid duration '{s}': {e}")))
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

    #[test]
    fn duration_compound_form() {
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            std::time::Duration::from_secs(5400)
        );
    }

    #[test]
    fn duration_long_unit_names() {
        assert_eq!(
            parse_duration("30sec").unwrap(),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            parse_duration("30 seconds").unwrap(),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn duration_default_values_parse() {
        // The CLI default_value strings must all parse.
        assert_eq!(
            parse_duration("60s").unwrap(),
            std::time::Duration::from_secs(60)
        );
        assert_eq!(
            parse_duration("5s").unwrap(),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration("10m").unwrap(),
            std::time::Duration::from_secs(600)
        );
    }

    #[test]
    fn duration_empty_is_invalid() {
        assert!(parse_duration("").is_err());
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
            lock_lease_duration: std::time::Duration::from_secs(60),
            lock_wait_timeout: std::time::Duration::from_secs(30),
            watch_mode: crate::watch::WatchMode::Off,
            watch_debounce: std::time::Duration::from_millis(500),
            cache_max_object_bytes: 12288,
            health_enabled: true,
            dry_run: false,
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
        assert_eq!(s, "https://a%2Fb:p%40ss@host/path");
    }

    #[test]
    fn embed_basic_auth_encodes_non_ascii_utf8() {
        // Non-ASCII bytes are percent-encoded per UTF-8 ("あ" = E3 81 82).
        let s = embed_basic_auth("https://host/p", "あ", "い").unwrap();
        assert_eq!(s, "https://%E3%81%82:%E3%81%84@host/p");
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
        // SAFETY: test-only; no concurrent test writes these env vars, so set_var/remove_var cannot race on environ.
        unsafe { std::env::set_var("LEANCD_TEST_AUTH_USER", "alice") };
        // SAFETY: test-only; no concurrent test writes these env vars, so set_var/remove_var cannot race on environ.
        unsafe { std::env::set_var("LEANCD_TEST_AUTH_PASS", "s3cr3t") };
        let mut cfg = test_config("https://github.com/o/r");
        cfg.git_username_env = "LEANCD_TEST_AUTH_USER".into();
        cfg.git_password_env = "LEANCD_TEST_AUTH_PASS".into();
        let url = cfg.authed_url().unwrap();
        // SAFETY: test-only; no concurrent test writes these env vars, so set_var/remove_var cannot race on environ.
        unsafe { std::env::remove_var("LEANCD_TEST_AUTH_USER") };
        // SAFETY: test-only; no concurrent test writes these env vars, so set_var/remove_var cannot race on environ.
        unsafe { std::env::remove_var("LEANCD_TEST_AUTH_PASS") };
        assert_eq!(url, "https://alice:s3cr3t@github.com/o/r");
    }

    #[test]
    fn effective_work_dir_appends_pid() {
        let cfg = test_config("https://github.com/o/r");
        let eff = cfg.effective_work_dir();
        assert!(
            eff.starts_with("/tmp/leancd-work-"),
            "effective_work_dir should keep the base work dir: {eff}"
        );
        assert!(
            eff.ends_with(&std::process::id().to_string()),
            "effective_work_dir should end with the pid: {eff}"
        );
    }

    #[test]
    fn effective_work_dir_strips_trailing_slash() {
        let mut cfg = test_config("https://github.com/o/r");
        cfg.work_dir = "/tmp/x/".into();
        let eff = cfg.effective_work_dir();
        assert!(!eff.contains("//"), "no double slash: {eff}");
        assert!(eff.starts_with("/tmp/x-"), "base kept without slash: {eff}");
        assert!(eff.ends_with(&std::process::id().to_string()));
    }
}
