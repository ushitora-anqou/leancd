//! Push manifest files into a Forgejo repo (over a port-forward).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::portforward::PortForward;
use crate::common::Forgejo;

/// Initialise a local git repo with `files`, commit, and force-push to the
/// named Forgejo repo over a transient port-forward. Returns the local clone
/// path (kept around so callers can append more commits and re-push).
pub fn push_files(forgejo: &Forgejo, repo: &str, files: &[(String, String)]) -> PathBuf {
    let pf = PortForward::new("forgejo", "svc/forgejo", 3000);
    let tmp = std::env::temp_dir().join(format!("leancd-e2e-{repo}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    git_in(&tmp, &["init", "-q", "-b", "main"]);
    write_files(&tmp, files);
    commit(&tmp);
    let authed = format!(
        "http://{}:{}@127.0.0.1:{}",
        forgejo.user(),
        forgejo.pass(),
        pf.local_port
    );
    let push_url = format!("{authed}/leancd/{repo}.git");
    let ps = push_url.as_str();
    git_in(&tmp, &["push", "-f", ps, "HEAD:main"]);
    tmp
}

/// Append more files to an existing local clone (from [`push_files`]) and
/// push a new commit.
pub fn push_more(clone: &Path, forgejo: &Forgejo, repo: &str, files: &[(String, String)]) {
    let pf = PortForward::new("forgejo", "svc/forgejo", 3000);
    write_files(clone, files);
    commit(clone);
    let authed = format!(
        "http://{}:{}@127.0.0.1:{}",
        forgejo.user(),
        forgejo.pass(),
        pf.local_port
    );
    let push_url = format!("{authed}/leancd/{repo}.git");
    let ps = push_url.as_str();
    git_in(clone, &["push", "-f", ps, "HEAD:main"]);
}

/// Remove the given files from an existing clone, commit the deletion, and
/// force-push. Used to exercise pruning.
pub fn remove_and_push(clone: &Path, forgejo: &Forgejo, repo: &str, files: &[String]) {
    let pf = PortForward::new("forgejo", "svc/forgejo", 3000);
    for f in files {
        let _ = fs::remove_file(clone.join(f));
    }
    git_in(clone, &["add", "-A"]);
    git_in(
        clone,
        &[
            "-c",
            "user.email=e2e@leancd",
            "-c",
            "user.name=e2e",
            "commit",
            "-qm",
            "remove",
        ],
    );
    let authed = format!(
        "http://{}:{}@127.0.0.1:{}",
        forgejo.user(),
        forgejo.pass(),
        pf.local_port
    );
    let push_url = format!("{authed}/leancd/{repo}.git");
    let ps = push_url.as_str();
    git_in(clone, &["push", "-f", ps, "HEAD:main"]);
}

fn write_files(root: &Path, files: &[(String, String)]) {
    for (name, content) in files {
        let path = root.join(name);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&path, content).unwrap();
    }
}

fn commit(dir: &Path) {
    git_in(dir, &["add", "-A"]);
    git_in(
        dir,
        &[
            "-c",
            "user.email=e2e@leancd",
            "-c",
            "user.name=e2e",
            "commit",
            "-qm",
            "e2e",
        ],
    );
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git spawn");
    assert!(
        status.success(),
        "git {:?} in {} failed",
        args,
        dir.display()
    );
}
