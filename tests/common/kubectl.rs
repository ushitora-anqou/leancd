//! Thin `kubectl` wrappers used by scenarios. An empty `ns` targets
//! cluster-scoped resources (the `-n` flag is omitted).

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Start a `kubectl get` command, omitting `-n` for cluster-scoped resources.
fn get_cmd(ns: &str, kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("kubectl");
    cmd.arg("get");
    if !ns.is_empty() {
        cmd.args(["-n", ns]);
    }
    cmd.args([kind, name]);
    cmd
}

/// True iff the named resource exists.
pub fn exists(ns: &str, kind: &str, name: &str) -> bool {
    get_cmd(ns, kind, name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `kubectl get -o json` parsed into a JSON value.
pub fn get_json(ns: &str, kind: &str, name: &str) -> Value {
    let mut cmd = get_cmd(ns, kind, name);
    cmd.args(["-o", "json"]);
    let out = cmd.output().expect("kubectl get -o json");
    assert!(
        out.status.success(),
        "kubectl get -o json {kind}/{name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse json")
}

/// `kubectl apply -f -` with the manifest on stdin.
pub fn apply_stdin(manifest: &str) {
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kubectl apply -f -");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(manifest.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "kubectl apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `kubectl delete` (ignores not-found).
pub fn delete(ns: &str, kind: &str, name: &str) {
    let mut cmd = Command::new("kubectl");
    cmd.arg("delete");
    if !ns.is_empty() {
        cmd.args(["-n", ns]);
    }
    cmd.args([kind, name, "--ignore-not-found=true"]);
    let _ = cmd.output();
}

/// `kubectl apply --server-side --field-manager <fm> -f -` (used to seed a
/// competing field manager that Lean CD must reclaim).
pub fn apply_ssa(manifest: &str, field_manager: &str) {
    let mut child = Command::new("kubectl")
        .args([
            "apply",
            "--server-side",
            "--field-manager",
            field_manager,
            "-f",
            "-",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kubectl apply --server-side");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(manifest.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "kubectl apply --server-side failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The first pod name in `ns` matching label `selector` (`kubectl get pods -l`),
/// or `None` when no pod matches. Used to resolve the Pod behind a controller
/// Job (`job-name=<job>`) or a Deployment (`app.kubernetes.io/name=<app>`).
pub fn pod_name_by_selector(ns: &str, selector: &str) -> Option<String> {
    let out = Command::new("kubectl")
        .args([
            "get",
            "pods",
            "-n",
            ns,
            "-l",
            selector,
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
