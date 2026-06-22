//! In-cluster Forgejo Git server lifecycle for the e2e suite.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::common::portforward::PortForward;
use crate::common::run;

/// Default admin credentials used to bootstrap the server and call its API.
const ADMIN_USER: &str = "leancd";
const ADMIN_PASS: &str = "leancd-e2e-pass";
const ADMIN_EMAIL: &str = "leancd@test.local";
const NS: &str = "forgejo";

/// A ready in-cluster Forgejo with an admin user.
pub struct Forgejo {
    user: &'static str,
    pass: &'static str,
}

impl Forgejo {
    /// Deploy Forgejo, wait until it is ready, and create the admin user.
    pub fn deploy() -> Forgejo {
        run(&["kubectl", "apply", "-f", "tests/forgejo.yaml"]);
        run(&[
            "kubectl",
            "wait",
            "-n",
            NS,
            "--for=condition=Available",
            "deploy/forgejo",
            "--timeout=240s",
        ]);
        wait_admin_cli();
        create_admin_user();
        Forgejo {
            user: ADMIN_USER,
            pass: ADMIN_PASS,
        }
    }

    pub fn user(&self) -> &str {
        self.user
    }
    pub fn pass(&self) -> &str {
        self.pass
    }

    /// In-cluster HTTPS clone URL for a repo owned by the admin user.
    pub fn https_url(&self, repo: &str) -> String {
        format!(
            "http://forgejo.{NS}.svc.cluster.local:3000/{}/{repo}.git",
            self.user
        )
    }

    /// In-cluster SSH clone URL for a repo owned by the admin user.
    pub fn ssh_url(&self, repo: &str) -> String {
        format!(
            "ssh://git@forgejo.{NS}.svc.cluster.local:22/{}/{repo}.git",
            self.user
        )
    }

    /// Create a private repo via the API (through a port-forward). When
    /// `auto_init` is set the repo is initialized with a README on a `main`
    /// branch, so it can be cloned immediately (used for the idle controller's
    /// repo); otherwise it is empty and the caller pushes the first commit.
    pub fn create_repo(&self, name: &str, auto_init: bool) {
        let body = format!(
            "{{\"name\":\"{name}\",\"private\":true,\"auto_init\":{auto_init},\"default_branch\":\"main\"}}"
        );
        let (ok, code) = self.api("POST", "/api/v1/user/repos", &body);
        assert!(ok, "create_repo {name} failed (http {code})");
    }

    /// Register an SSH public key for the admin user.
    pub fn add_ssh_key(&self, pub_key: &str) {
        let escaped = json_escape(pub_key);
        let body = format!("{{\"title\":\"e2e\",\"key\":\"{escaped}\"}}");
        let (ok, code) = self.api("POST", "/api/v1/user/keys", &body);
        assert!(ok, "add_ssh_key failed (http {code})");
    }

    /// Call a Forgejo API endpoint over a transient port-forward with basic
    /// auth. Retries while curl cannot get an HTTP response (port-forward
    /// warming up); returns `(ok, http_code)`.
    fn api(&self, method: &str, path: &str, body: &str) -> (bool, String) {
        let pf = PortForward::new(NS, "svc/forgejo", 3000);
        let url = format!("http://127.0.0.1:{}{path}", pf.local_port);
        let auth = format!("{}:{}", self.user, self.pass);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let out = Command::new("curl")
                .args([
                    "-sS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "-X",
                    method,
                    "-u",
                    &auth,
                    "-H",
                    "Content-Type: application/json",
                    "-d",
                    body,
                    &url,
                ])
                .output()
                .expect("curl");
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if code != "000" {
                let ok = matches!(code.as_str(), "200" | "201");
                return (ok, code);
            }
            if Instant::now() > deadline {
                return (false, code);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

/// Wait until `forgejo admin user list` succeeds (DB initialized).
fn wait_admin_cli() {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if admin_user_list().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!("forgejo admin CLI did not become ready in 120s");
}

fn admin_user_list() -> Result<String, ()> {
    let out = Command::new("kubectl")
        .args([
            "exec",
            "-n",
            NS,
            "deploy/forgejo",
            "--",
            "su",
            "-c",
            "forgejo admin user list -c /data/gitea/conf/app.ini -w /var/lib/gitea",
            "git",
        ])
        .output()
        .map_err(|e| {
            eprintln!("admin_user_list spawn error: {e}");
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        eprintln!(
            "admin_user_list failed (exit {:?}):\n--- stderr ---\n{}\n--- stdout ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        Err(())
    }
}

fn create_admin_user() {
    if admin_user_list().unwrap_or_default().contains(ADMIN_USER) {
        return;
    }
    // Run as the `git` user (root would corrupt file ownership) and point the
    // CLI at the config + work dir it writes under in the container image.
    let cmd = format!(
        "forgejo admin user create --admin --username {ADMIN_USER} --password {ADMIN_PASS} \
         --email {ADMIN_EMAIL} --must-change-password=false \
         -c /data/gitea/conf/app.ini -w /var/lib/gitea"
    );
    let out = Command::new("kubectl")
        .args([
            "exec",
            "-n",
            NS,
            "deploy/forgejo",
            "--",
            "su",
            "-c",
            &cmd,
            "git",
        ])
        .output()
        .expect("kubectl exec forgejo admin user create");
    assert!(
        out.status.success(),
        "forgejo admin user create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Minimal JSON string escaper for embedding a public key into a JSON body.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
