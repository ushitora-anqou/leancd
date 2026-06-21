//! Helm install/uninstall helpers. The host `helm` binary drives the same
//! kubeconfig that `kubectl` uses against the kind cluster.

use std::process::Command;

/// `helm install <release> charts/leancd --namespace <ns> --create-namespace
/// [--set <k=v>...]`. Panics on failure (mirrors [`crate::common::fixture::run`]).
pub fn install(release: &str, namespace: &str, set_args: &[&str]) {
    let mut args: Vec<String> = vec![
        "helm".into(),
        "install".into(),
        release.into(),
        "charts/leancd".into(),
        "--namespace".into(),
        namespace.into(),
        "--create-namespace".into(),
    ];
    for &kv in set_args {
        args.push("--set".into());
        args.push(kv.into());
    }
    eprintln!(">> {}", args.join(" "));
    let (prog, rest) = args.split_first().expect("non-empty args");
    let output = Command::new(prog)
        .args(rest)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn helm: {e}"));
    assert!(
        output.status.success(),
        "helm install failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

/// `helm uninstall <release> --namespace <ns>`. Ignores failure: best-effort
/// cleanup (the kind cluster is torn down at process exit regardless).
pub fn uninstall(release: &str, namespace: &str) {
    let _ = Command::new("helm")
        .args(["uninstall", release, "--namespace", namespace])
        .output();
}
