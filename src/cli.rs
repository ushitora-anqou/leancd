//! Command-line interface definition (clap).

use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::config::{parse_duration, Config};
use crate::error::Result;

/// Lean CD — a minimal, low-memory Kubernetes CD controller.
#[derive(Debug, Parser)]
#[command(name = "leancd", version = crate::version::FULL_VERSION, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run as a long-lived controller (deploy as a Deployment).
    Controller(CommonArgs),
    /// Perform a single reconciliation pass and exit.
    Sync(CommonArgs),
    /// Print the last known sync state.
    Status(CommonArgs),
    /// Check sync health and exit (for liveness/readiness exec probes).
    Health(CommonArgs),
}

/// Flags shared by all subcommands. These fully configure a single
/// repository-sync target.
#[derive(Debug, Args, Clone)]
pub struct CommonArgs {
    /// Git repository URL.
    #[arg(long, env = "LEANCD_REPO_URL")]
    pub repo_url: String,

    /// Branch to track.
    #[arg(long, env = "LEANCD_BRANCH", default_value = "main")]
    pub branch: String,

    /// Glob patterns of directories to sync, scanned recursively (e.g.
    /// `live/*/prod`). `*` matches one path segment, `**` matches any depth.
    /// Repeatable (`--path a --path b`) and comma-separated when set via
    /// `LEANCD_PATH` (`LEANCD_PATH=a,b`). Defaults to the whole repo (`.`).
    #[arg(long, env = "LEANCD_PATH", value_delimiter = ',')]
    pub path: Vec<String>,

    /// Polling interval (e.g. 30s, 5m, or a compound form like 1h30m).
    #[arg(long, env = "LEANCD_POLL_INTERVAL", default_value = "60s")]
    pub poll_interval: String,

    /// Namespace Lean CD runs in.
    #[arg(long, env = "LEANCD_NAMESPACE", default_value = "default")]
    pub namespace: String,

    /// Name of the ConfigMap holding sync state.
    #[arg(long, env = "LEANCD_STATE_CONFIGMAP", default_value = "leancd-state")]
    pub state_configmap: String,

    /// Local working directory for the checkout.
    #[arg(long, env = "LEANCD_WORK_DIR", default_value = "/tmp/leancd-work")]
    pub work_dir: String,

    /// Env var holding the git HTTPS username (injected from a Secret).
    #[arg(long, env = "LEANCD_GIT_USERNAME_ENV", default_value = "GIT_USERNAME")]
    pub git_username_env: String,

    /// Env var holding the git HTTPS password/token (injected from a Secret).
    #[arg(long, env = "LEANCD_GIT_PASSWORD_ENV", default_value = "GIT_PASSWORD")]
    pub git_password_env: String,

    /// Env var holding the SSH private key (PEM) for SSH repos.
    #[arg(long, env = "LEANCD_GIT_SSH_KEY_ENV", default_value = "GIT_SSH_KEY")]
    pub git_ssh_key_env: String,

    /// Managed-by label key.
    #[arg(long, default_value = "app.kubernetes.io/managed-by")]
    pub managed_label_key: String,

    /// Managed-by label value.
    #[arg(long, default_value = "leancd")]
    pub managed_label_value: String,

    /// SSA field manager name.
    #[arg(long, default_value = "leancd")]
    pub field_manager: String,

    /// Per-hook completion timeout in seconds (Job/Pod hooks). A hook that does
    /// not reach a terminal state within this window is treated as failed.
    #[arg(long, env = "LEANCD_HOOK_TIMEOUT_SECS", default_value = "300")]
    pub hook_timeout_secs: u64,

    /// Base delay for exponential backoff after a failed reconciliation pass
    /// (e.g. `5s`). The delay doubles each consecutive failure, capped at
    /// `--backoff-max`, and resets to the poll interval on success.
    #[arg(long, env = "LEANCD_BACKOFF_BASE", default_value = "5s")]
    pub backoff_base: String,

    /// Maximum backoff delay between retries (e.g. `10m`).
    #[arg(long, env = "LEANCD_BACKOFF_MAX", default_value = "10m")]
    pub backoff_max: String,

    /// Seconds to wait for an in-flight reconciliation pass to finish on
    /// shutdown before force-aborting it. Must fit within the Pod's
    /// `terminationGracePeriodSeconds`.
    #[arg(long, env = "LEANCD_SHUTDOWN_TIMEOUT_SECS", default_value = "28")]
    pub shutdown_timeout_secs: u64,

    /// A sync is reported stale by `leancd health` when the last successful
    /// sync is older than `poll_interval` times this factor.
    #[arg(long, env = "LEANCD_HEALTH_STALE_FACTOR", default_value = "10")]
    pub health_stale_factor: u32,

    /// Lifetime (seconds) of the reconcile-exclusion Lease (`lock.rs`). A
    /// crashed holder's lease is forcibly acquired by another process after
    /// this duration. Must exceed the longest normal reconcile pass (including
    /// hook waits) and `--shutdown-timeout-secs`.
    #[arg(long, env = "LEANCD_LOCK_LEASE_DURATION_SECS", default_value = "60")]
    pub lock_lease_duration_secs: u64,

    /// Seconds to wait for the reconcile Lease when another pass already holds
    /// it before skipping this pass with a "busy" INFO log (not an error).
    #[arg(long, env = "LEANCD_LOCK_WAIT_TIMEOUT_SECS", default_value = "30")]
    pub lock_wait_timeout_secs: u64,

    /// How cluster-side drift is detected: `off` (periodic poll only), `trigger`
    /// (watch wakes the loop; drift checked via the existing List), or `cache`
    /// (watch + Store cache; drift read from the cache, so no per-pass List).
    /// See `--watch-debounce`. Defaults to `cache` — measured (see `bench/`) to
    /// stay well under the RSS budget while avoiding per-poll apiserver Lists.
    #[arg(long, env = "LEANCD_WATCH_MODE", default_value = "cache")]
    pub watch_mode: String,

    /// Debounce window for watch-triggered reconciles (e.g. `500ms`): a burst of
    /// watch events within this window collapses into one pass, so a reconnect's
    /// InitApply burst or a rapid edit storm does not trigger N passes.
    #[arg(long, env = "LEANCD_WATCH_DEBOUNCE", default_value = "500ms")]
    pub watch_debounce: String,

    /// In `cache` watch mode, the maximum serialized size (bytes) of an object
    /// cached in full; larger objects are tracked by key only and drift-checked
    /// via a per-GVK `List` fallback (size-based, any kind). `0` disables body
    /// caching entirely (all LargeTier).
    #[arg(long, env = "LEANCD_CACHE_MAX_OBJECT_BYTES", default_value = "12288")]
    pub cache_max_object_bytes: usize,

    /// Whether to evaluate and publish resource health: `on` (default) runs the
    /// Argo CD-style health assessment each pass; `off` skips it (and its
    /// metric). Sync completion is unaffected — health is an independent signal.
    #[arg(long, env = "LEANCD_HEALTH_MODE", default_value = "on")]
    pub health_mode: String,
}

impl CommonArgs {
    /// Convert flags into a validated [`Config`].
    pub fn to_config(&self) -> Result<Config> {
        let poll_interval: Duration = parse_duration(&self.poll_interval)?;
        Ok(Config {
            repo_url: self.repo_url.clone(),
            branch: self.branch.clone(),
            path: if self.path.is_empty() {
                vec![".".to_string()]
            } else {
                self.path.clone()
            },
            poll_interval,
            namespace: self.namespace.clone(),
            state_configmap: self.state_configmap.clone(),
            work_dir: self.work_dir.clone(),
            git_username_env: self.git_username_env.clone(),
            git_password_env: self.git_password_env.clone(),
            git_ssh_key_env: self.git_ssh_key_env.clone(),
            managed_label_key: self.managed_label_key.clone(),
            managed_label_value: self.managed_label_value.clone(),
            field_manager: self.field_manager.clone(),
            hook_timeout: Duration::from_secs(self.hook_timeout_secs),
            backoff_base: parse_duration(&self.backoff_base)?,
            backoff_max: parse_duration(&self.backoff_max)?,
            shutdown_timeout: Duration::from_secs(self.shutdown_timeout_secs),
            health_stale_factor: self.health_stale_factor,
            lock_lease_duration: Duration::from_secs(self.lock_lease_duration_secs),
            lock_wait_timeout: Duration::from_secs(self.lock_wait_timeout_secs),
            watch_mode: crate::watch::WatchMode::parse(&self.watch_mode)?,
            watch_debounce: parse_duration(&self.watch_debounce)?,
            cache_max_object_bytes: self.cache_max_object_bytes,
            health_enabled: self.health_mode.eq_ignore_ascii_case("on"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(hook_secs: u64) -> CommonArgs {
        CommonArgs {
            repo_url: "https://example.com/o/r".into(),
            branch: "main".into(),
            path: vec![],
            poll_interval: "60s".into(),
            namespace: "default".into(),
            state_configmap: "leancd-state".into(),
            work_dir: "/tmp/leancd-work".into(),
            git_username_env: "GIT_USERNAME".into(),
            git_password_env: "GIT_PASSWORD".into(),
            git_ssh_key_env: "GIT_SSH_KEY".into(),
            managed_label_key: "app.kubernetes.io/managed-by".into(),
            managed_label_value: "leancd".into(),
            field_manager: "leancd".into(),
            hook_timeout_secs: hook_secs,
            backoff_base: "5s".into(),
            backoff_max: "10m".into(),
            shutdown_timeout_secs: 28,
            health_stale_factor: 10,
            lock_lease_duration_secs: 60,
            lock_wait_timeout_secs: 30,
            watch_mode: "off".into(),
            watch_debounce: "500ms".into(),
            cache_max_object_bytes: 12288,
            health_mode: "on".into(),
        }
    }

    #[test]
    fn hook_timeout_maps_seconds_to_duration() {
        assert_eq!(
            common(300).to_config().unwrap().hook_timeout,
            Duration::from_secs(300)
        );
        assert_eq!(
            common(0).to_config().unwrap().hook_timeout,
            Duration::from_secs(0)
        );
    }

    #[test]
    fn watch_mode_parses_valid_variants() {
        let mut a = common(300);
        a.watch_mode = "trigger".into();
        assert_eq!(
            a.to_config().unwrap().watch_mode,
            crate::watch::WatchMode::Trigger
        );
        let mut a = common(300);
        a.watch_mode = "CACHE".into();
        assert_eq!(
            a.to_config().unwrap().watch_mode,
            crate::watch::WatchMode::Cache
        );
        let mut a = common(300);
        a.watch_mode = "off".into();
        assert_eq!(
            a.to_config().unwrap().watch_mode,
            crate::watch::WatchMode::Off
        );
    }

    #[test]
    fn watch_mode_rejects_invalid() {
        let mut a = common(300);
        a.watch_mode = "bogus".into();
        assert!(a.to_config().is_err());
    }

    #[test]
    fn watch_debounce_parses_duration() {
        let mut a = common(300);
        a.watch_debounce = "750ms".into();
        assert_eq!(
            a.to_config().unwrap().watch_debounce,
            Duration::from_millis(750)
        );
    }

    #[test]
    fn cache_max_object_bytes_defaults_to_threshold() {
        assert_eq!(
            common(300).to_config().unwrap().cache_max_object_bytes,
            12288
        );
    }

    #[test]
    fn cache_max_object_bytes_is_configurable() {
        let mut a = common(300);
        a.cache_max_object_bytes = 4096;
        assert_eq!(a.to_config().unwrap().cache_max_object_bytes, 4096);
    }

    #[test]
    fn cache_max_object_bytes_reads_from_env() {
        // clap's `env =` attribute fills the flag from
        // LEANCD_CACHE_MAX_OBJECT_BYTES when --cache-max-object-bytes is absent.
        std::env::set_var("LEANCD_CACHE_MAX_OBJECT_BYTES", "8192");
        let cli = Cli::try_parse_from(["leancd", "sync", "--repo-url", "https://x"]).unwrap();
        std::env::remove_var("LEANCD_CACHE_MAX_OBJECT_BYTES");
        let args = match cli.command {
            Command::Sync(a) => a,
            _ => panic!("expected the sync subcommand"),
        };
        assert_eq!(args.cache_max_object_bytes, 8192);
    }

    #[test]
    fn cache_max_object_bytes_rejects_non_numeric() {
        // usize parse failure is surfaced by clap as an error (not a Config
        // error): `--cache-max-object-bytes abc` must not build a Config.
        let err = Cli::try_parse_from([
            "leancd",
            "sync",
            "--repo-url",
            "https://x",
            "--cache-max-object-bytes",
            "not-a-number",
        ]);
        assert!(
            err.is_err(),
            "non-numeric --cache-max-object-bytes must be rejected"
        );
    }
}
