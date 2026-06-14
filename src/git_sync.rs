//! Git synchronization via the `git` CLI (shallow, polling-based).
//!
//! Per the design we shell out to `git` rather than embed a Git library: git
//! runs as a separate process, so its memory is not counted in leancd's RSS
//! (the headline memory budget). A depth-1 shallow checkout keeps the working
//! tree small; HEAD SHA comparison drives change detection.
//!
//! Both HTTPS (basic auth embedded in the URL) and SSH (an injected private
//! key, passed to git via `GIT_SSH_COMMAND`) transports are supported. The SSH
//! key is written to a 0600 temp file for the duration of a sync and removed
//! when the handle is dropped, so it never lives in process memory long-term.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::Config;
use crate::error::{Error, Result};

/// Result of one git synchronization pass.
#[derive(Debug, Clone)]
pub struct SyncOutcome {
    /// The checked-out commit SHA.
    pub sha: String,
    /// Whether the SHA differs from the previously known one.
    pub changed: bool,
}

/// Fetch (or initially clone) the repository and report the resulting HEAD.
///
/// `prev_sha` is the last known HEAD; when it differs from the freshly fetched
/// SHA, `changed` is `true` and the caller can skip work when nothing moved.
pub async fn sync(cfg: &Config, prev_sha: Option<&str>) -> Result<SyncOutcome> {
    let url = cfg.authed_url()?;
    let work = Path::new(&cfg.work_dir);
    let git_dir = work.join(".git");

    // If an SSH key is configured, materialize it to a temp file so the git
    // subprocess can use it. The files are unlinked when `ssh` is dropped.
    let ssh = SshKeyFile::prepare(cfg, work).await?;

    if git_dir.exists() {
        // Existing checkout: update in place.
        run_git(
            &[
                "-C",
                &cfg.work_dir,
                "fetch",
                "--depth",
                "1",
                &url,
                &cfg.branch,
            ],
            false,
            ssh.as_ref(),
        )
        .await?;
        run_git(
            &["-C", &cfg.work_dir, "reset", "--hard", "FETCH_HEAD"],
            false,
            ssh.as_ref(),
        )
        .await?;
    } else {
        // Fresh shallow clone.
        if work.exists() {
            tokio::fs::remove_dir_all(&cfg.work_dir)
                .await
                .map_err(|e| Error::Git(format!("could not clear stale work dir: {e}")))?;
        }
        run_git(
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                &cfg.branch,
                &url,
                &cfg.work_dir,
            ],
            false,
            ssh.as_ref(),
        )
        .await?;
    }

    let sha = run_git_capture(&["-C", &cfg.work_dir, "rev-parse", "HEAD"], ssh.as_ref()).await?;
    let sha = sha.trim().to_string();
    let changed = match prev_sha {
        Some(prev) => prev != sha,
        None => true,
    };
    Ok(SyncOutcome { sha, changed })
}

/// An SSH private key plus a per-process `known_hosts` file, written to disk
/// for the duration of a sync so the git subprocess can authenticate over SSH.
/// Both files are removed when this is dropped.
struct SshKeyFile {
    key_path: PathBuf,
    known_hosts: PathBuf,
}

impl SshKeyFile {
    /// Write the configured SSH key (if any) to disk and return a handle. When
    /// no key is configured this returns `Ok(None)` and SSH is left to git's
    /// defaults (e.g. the user's agent or `~/.ssh`).
    async fn prepare(cfg: &Config, work: &Path) -> Result<Option<Self>> {
        let key = match cfg.ssh_private_key() {
            Some(k) => k,
            None => return Ok(None),
        };
        let parent = work.parent().unwrap_or_else(|| Path::new("/tmp"));
        // Best-effort: the parent (e.g. /tmp) almost always exists already.
        let _ = tokio::fs::create_dir_all(parent).await;
        // PID-scoped names so concurrent leancd processes do not clash.
        let tag = std::process::id();
        let key_path = parent.join(format!(".leancd_ssh_key_{tag}"));
        let known_hosts = parent.join(format!(".leancd_known_hosts_{tag}"));

        tokio::fs::write(&key_path, key.as_bytes())
            .await
            .map_err(|e| Error::Git(format!("could not write ssh key file: {e}")))?;
        set_mode_0600(&key_path)?;
        // An empty known_hosts lets ssh append (StrictHostKeyChecking=accept-new).
        if tokio::fs::metadata(&known_hosts).await.is_err() {
            tokio::fs::File::create(&known_hosts)
                .await
                .map_err(|e| Error::Git(format!("could not create known_hosts: {e}")))?;
            set_mode_0600(&known_hosts)?;
        }
        Ok(Some(Self {
            key_path,
            known_hosts,
        }))
    }

    /// The value to place in `GIT_SSH_COMMAND`.
    fn command(&self) -> String {
        format_ssh_command(&self.key_path, &self.known_hosts)
    }
}

impl Drop for SshKeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.key_path);
        let _ = std::fs::remove_file(&self.known_hosts);
    }
}

/// Build the `GIT_SSH_COMMAND` value: use the injected key, accept new host
/// keys on first contact, and isolate the known-hosts store to a per-process
/// file so leancd never touches the user's `~/.ssh/known_hosts`.
fn format_ssh_command(key_path: &Path, known_hosts: &Path) -> String {
    format!(
        "ssh -i {} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
        key_path.display(),
        known_hosts.display()
    )
}

/// Run a git command, returning an error if it exits non-zero.
async fn run_git(args: &[&str], capture: bool, ssh: Option<&SshKeyFile>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0"); // never prompt for credentials
    cmd.env("GIT_HTTP_USER_AGENT", "leancd");
    if let Some(ssh) = ssh {
        cmd.env("GIT_SSH_COMMAND", ssh.command());
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| Error::Git(format!("failed to invoke git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "git {:?} failed: {}",
            args,
            stderr.trim()
        )));
    }
    if capture {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Ok(String::new())
    }
}

async fn run_git_capture(args: &[&str], ssh: Option<&SshKeyFile>) -> Result<String> {
    run_git(args, true, ssh).await
}

/// Restrict a file to owner-only access (required by ssh for private keys).
#[cfg(unix)]
fn set_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Git(format!("could not chmod 0600 {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn ssh_command_references_key_and_known_hosts() {
        let cmd = format_ssh_command(Path::new("/tmp/key"), Path::new("/tmp/known_hosts"));
        assert!(
            cmd.contains("-i /tmp/key"),
            "command must reference the key file"
        );
        assert!(
            cmd.contains("StrictHostKeyChecking=accept-new"),
            "command must accept new host keys"
        );
        assert!(
            cmd.contains("UserKnownHostsFile=/tmp/known_hosts"),
            "command must pin the known_hosts file"
        );
    }

    /// Run a `git` command inside `dir`, asserting it succeeds.
    fn git_in(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git failed to spawn");
        assert!(
            status.success(),
            "git {:?} in {} failed",
            args,
            dir.display()
        );
    }

    /// End-to-end check that `sync` clones a repo and detects HEAD changes.
    /// Ignored by default (needs `git` on PATH); run with
    /// `cargo test sync_roundtrip -- --ignored`.
    #[tokio::test]
    #[ignore = "requires git on PATH; run with: cargo test -- --ignored"]
    async fn sync_roundtrip_detects_changes() {
        let tmp = std::env::temp_dir().join(format!("leancd-gitsync-test-{}", std::process::id()));
        // Probe git with the same `status()` call the body uses, so a sandboxed
        // or git-less host skips cleanly instead of failing.
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            eprintln!("skipping: git not available in this environment");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        let work = tmp.join("work");
        std::fs::create_dir_all(&repo).unwrap();

        // A one-commit repo on `main`.
        git_in(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join("cm.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "v1",
            ],
        );

        let cfg = Config {
            repo_url: format!("file://{}", repo.display()),
            branch: "main".into(),
            work_dir: work.to_string_lossy().into(),
            ..Default::default()
        };

        let first = sync(&cfg, None).await.unwrap();
        assert!(first.changed, "first sync must report a change");
        assert!(!first.sha.is_empty());
        assert!(work.join(".git").exists(), "clone must create a checkout");

        // Re-running against the same SHA must report no change.
        let second = sync(&cfg, Some(&first.sha)).await.unwrap();
        assert!(
            !second.changed,
            "an unchanged HEAD must not report a change"
        );
        assert_eq!(first.sha, second.sha);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
