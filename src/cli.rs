//! Command-line interface definition (clap).

use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::config::{parse_duration, Config};
use crate::error::Result;

/// Lean CD — a minimal, low-memory Kubernetes CD controller.
#[derive(Debug, Parser)]
#[command(name = "leancd", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run as a long-lived controller (deploy as a Deployment).
    Controller(CommonArgs),
    /// Perform a single reconciliation pass and exit.
    Sync {
        #[command(flatten)]
        common: CommonArgs,
        /// Force server-side-apply to take ownership of conflicting fields.
        #[arg(long)]
        force: bool,
    },
    /// Print the last known sync state.
    Status(CommonArgs),
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

    /// Polling interval (e.g. 30s, 5m).
    #[arg(long, env = "LEANCD_POLL_INTERVAL", default_value = "60s")]
    pub poll_interval: String,

    /// Namespace leancd runs in.
    #[arg(long, env = "LEANCD_NAMESPACE", default_value = "default")]
    pub namespace: String,

    /// Name of the ConfigMap holding sync state.
    #[arg(long, env = "LEANCD_STATE_CONFIGMAP", default_value = "leancd-state")]
    pub state_configmap: String,

    /// Address for the Prometheus metrics endpoint.
    #[arg(long, env = "LEANCD_METRICS_ADDR", default_value = "0.0.0.0:9090")]
    pub metrics_addr: String,

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
            metrics_addr: self.metrics_addr.clone(),
            work_dir: self.work_dir.clone(),
            git_username_env: self.git_username_env.clone(),
            git_password_env: self.git_password_env.clone(),
            git_ssh_key_env: self.git_ssh_key_env.clone(),
            managed_label_key: self.managed_label_key.clone(),
            managed_label_value: self.managed_label_value.clone(),
            field_manager: self.field_manager.clone(),
        })
    }
}
