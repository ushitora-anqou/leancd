//! Git synchronization via the `git` CLI (shallow fetch/clone).
//!
//! We shell out to `git` rather than embed a Git library: the `git` CLI gives
//! reliable repeated shallow fetches and resets through a simple, battle-tested
//! API, with no need to re-implement that machinery in-process. A depth-1
//! shallow checkout keeps the working tree small; HEAD SHA comparison drives
//! change detection.
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

/// What a `checkout` should leave the working tree pointing at.
#[derive(Debug, Clone)]
pub enum CheckoutTarget {
    /// The configured branch HEAD — the normal reconcile path.
    Branch,
    /// A specific commit SHA (used by `rollback --to <sha>`).
    Sha(String),
    /// The Nth first-parent ancestor of the fetched branch HEAD (1 = the
    /// previous commit); used by `rollback` with no `--to`.
    Parent(u32),
}

impl CheckoutTarget {
    /// `--to <sha>` selects [`CheckoutTarget::Sha`]; no `--to` selects
    /// [`CheckoutTarget::Parent`] with depth 1 (the previous commit).
    pub fn from_opt(to: Option<&str>) -> Self {
        match to {
            Some(sha) => CheckoutTarget::Sha(sha.to_string()),
            None => CheckoutTarget::Parent(1),
        }
    }
}

/// Fetch (or initially clone) the repository and leave the working tree at
/// `target` (the branch HEAD by default, a specific SHA, or an ancestor).
///
/// `prev_sha` is the last known HEAD; when it differs from the freshly
/// resolved SHA, `changed` is `true`. A non-branch target deepens the shallow
/// clone in batches until it is reachable (capped, so a bogus SHA fails fast).
pub async fn checkout(
    cfg: &Config,
    prev_sha: Option<&str>,
    target: &CheckoutTarget,
) -> Result<SyncOutcome> {
    let url = cfg.authed_url()?;
    // PID-scoped so the controller process and a concurrent `leancd sync` (e.g.
    // via `kubectl exec` in the same Pod) never operate on the same shallow
    // checkout at once. See `Config::effective_work_dir` and `lock.rs`.
    let work_dir = cfg.effective_work_dir();
    let work = Path::new(&work_dir);
    let git_dir = work.join(".git");

    // If an SSH key is configured, materialize it to a temp file so the git
    // subprocess can use it. The files are unlinked when `ssh` is dropped.
    let ssh = SshKeyFile::prepare(cfg, work).await?;

    if git_dir.exists() {
        // Existing checkout: update the branch tip in place.
        run_git(
            &["-C", &work_dir, "fetch", "--depth", "1", &url, &cfg.branch],
            false,
            ssh.as_ref(),
        )
        .await?;
        // A branch target fast-forwards to the freshly fetched tip; a SHA or
        // ancestor target is checked out below (after deepening as needed).
        if matches!(target, CheckoutTarget::Branch) {
            run_git(
                &["-C", &work_dir, "reset", "--hard", "FETCH_HEAD"],
                false,
                ssh.as_ref(),
            )
            .await?;
        }
    } else {
        // Fresh shallow clone of the tracked branch (already checked out).
        if work.exists() {
            tokio::fs::remove_dir_all(&work_dir)
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
                &work_dir,
            ],
            false,
            ssh.as_ref(),
        )
        .await?;
    }

    // Point the working tree at the requested target.
    let sha = match target {
        CheckoutTarget::Branch => {
            // Already at the branch tip (fetch+reset above, or a fresh clone).
            run_git_capture(&["-C", &work_dir, "rev-parse", "HEAD"], ssh.as_ref())
                .await?
                .trim()
                .to_string()
        }
        CheckoutTarget::Sha(sha) => {
            ensure_sha_reachable(&work_dir, sha, &url, &cfg.branch, ssh.as_ref()).await?;
            run_git(&["-C", &work_dir, "checkout", sha], false, ssh.as_ref()).await?;
            run_git(
                &["-C", &work_dir, "reset", "--hard", sha],
                false,
                ssh.as_ref(),
            )
            .await?;
            sha.clone()
        }
        CheckoutTarget::Parent(n) => {
            ensure_depth(&work_dir, *n as usize + 1, &url, &cfg.branch, ssh.as_ref()).await?;
            let ref_s = format!("FETCH_HEAD~{n}");
            run_git(&["-C", &work_dir, "checkout", &ref_s], false, ssh.as_ref()).await?;
            run_git(
                &["-C", &work_dir, "reset", "--hard", &ref_s],
                false,
                ssh.as_ref(),
            )
            .await?;
            run_git_capture(&["-C", &work_dir, "rev-parse", "HEAD"], ssh.as_ref())
                .await?
                .trim()
                .to_string()
        }
    };

    let changed = match prev_sha {
        Some(prev) => prev != sha,
        None => true,
    };
    Ok(SyncOutcome { sha, changed })
}

/// Fetch the branch HEAD (the normal reconcile path). Equivalent to
/// [`checkout`] with [`CheckoutTarget::Branch`].
pub async fn sync(cfg: &Config, prev_sha: Option<&str>) -> Result<SyncOutcome> {
    checkout(cfg, prev_sha, &CheckoutTarget::Branch).await
}

/// Ensure `sha` is present in the shallow checkout, deepening history in
/// batches until it is found (capped so a bogus SHA does not fetch forever).
async fn ensure_sha_reachable(
    work_dir: &str,
    sha: &str,
    url: &str,
    branch: &str,
    ssh: Option<&SshKeyFile>,
) -> Result<()> {
    for _ in 0..20 {
        if run_git(&["-C", work_dir, "cat-file", "-e", sha], true, ssh)
            .await
            .is_ok()
        {
            return Ok(());
        }
        run_git(
            &["-C", work_dir, "fetch", "--deepen=50", url, branch],
            false,
            ssh,
        )
        .await?;
    }
    Err(Error::Git(format!(
        "commit {sha} not reachable after deepening history; check the SHA and branch"
    )))
}

/// Ensure the shallow checkout holds at least `depth` commits so an ancestor
/// ref (`FETCH_HEAD~N`) resolves. Deepens in batches, capped.
async fn ensure_depth(
    work_dir: &str,
    depth: usize,
    url: &str,
    branch: &str,
    ssh: Option<&SshKeyFile>,
) -> Result<()> {
    for _ in 0..20 {
        let count = run_git_capture(&["-C", work_dir, "rev-list", "--count", "HEAD"], ssh).await?;
        if count.trim().parse::<usize>().unwrap_or(0) >= depth {
            return Ok(());
        }
        run_git(
            &["-C", work_dir, "fetch", "--deepen=50", url, branch],
            false,
            ssh,
        )
        .await?;
    }
    Err(Error::Git(format!(
        "could not reach depth {depth} after deepening history"
    )))
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
        // PID-scoped names so concurrent Lean CD processes do not clash.
        let tag = std::process::id();
        let key_path = parent.join(format!(".leancd_ssh_key_{tag}"));
        let known_hosts = parent.join(format!(".leancd_known_hosts_{tag}"));

        // The key is trimmed when read from the env, but ssh/OpenSSH requires a
        // trailing newline after the PEM footer ("error in libcrypto"
        // otherwise), so add it back when materialising the file.
        let key_with_newline = format!("{key}\n");
        tokio::fs::write(&key_path, key_with_newline.as_bytes())
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
/// file so Lean CD never touches the user's `~/.ssh/known_hosts`.
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
        assert!(
            Path::new(&cfg.effective_work_dir()).join(".git").exists(),
            "clone must create a checkout"
        );

        // Re-running against the same SHA must report no change.
        let second = sync(&cfg, Some(&first.sha)).await.unwrap();
        assert!(
            !second.changed,
            "an unchanged HEAD must not report a change"
        );
        assert_eq!(first.sha, second.sha);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn checkout_target_from_opt_maps_none_to_parent() {
        assert!(matches!(
            CheckoutTarget::from_opt(None),
            CheckoutTarget::Parent(1)
        ));
    }

    #[test]
    fn checkout_target_from_opt_maps_some_to_sha() {
        match CheckoutTarget::from_opt(Some("abc123")) {
            CheckoutTarget::Sha(s) => assert_eq!(s, "abc123"),
            other => panic!("expected Sha, got {other:?}"),
        }
    }

    /// Roll back to the previous commit (HEAD^): `checkout` with `Parent(1)`
    /// resolves the first commit's SHA. Ignored (needs git). Run with
    /// `cargo test checkout_rollback -- --ignored`.
    #[tokio::test]
    #[ignore = "requires git on PATH; run with: cargo test -- --ignored"]
    async fn checkout_rollback_to_parent_resolves_previous_commit() {
        let tmp = std::env::temp_dir().join(format!(
            "leancd-gitsync-rollback-test-{}",
            std::process::id()
        ));
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

        git_in(&repo, &["init", "-q", "-b", "main"]);
        // First commit.
        std::fs::write(
            repo.join("a.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n",
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
        // Second commit becomes the branch tip.
        std::fs::write(
            repo.join("b.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\n",
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
                "v2",
            ],
        );

        let cfg = Config {
            repo_url: format!("file://{}", repo.display()),
            branch: "main".into(),
            work_dir: work.to_string_lossy().into(),
            ..Default::default()
        };

        // Roll back one commit: the resolved SHA is v1, and it is "changed".
        let outcome = checkout(&cfg, None, &CheckoutTarget::Parent(1))
            .await
            .unwrap();
        let v1 = std::process::Command::new("git")
            .args(["-C", &repo.to_string_lossy(), "rev-parse", "main~1"])
            .output()
            .unwrap();
        let v1 = String::from_utf8_lossy(&v1.stdout).trim().to_string();
        assert_eq!(
            outcome.sha, v1,
            "rollback should land on the previous commit"
        );
        assert!(outcome.changed, "rollback must report a change");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
