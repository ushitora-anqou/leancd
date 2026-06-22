//! Ephemeral kind cluster + Lean CD image + in-cluster Forgejo, shared across
//! all scenarios via a process-wide `OnceLock`. `Drop` tears the cluster down
//! (Pods included).

use std::process::Command;
use std::sync::OnceLock;

use crate::common::forgejo::Forgejo;

/// Name of the e2e kind cluster (kept distinct from `make bench`'s cluster).
pub const CLUSTER_NAME: &str = "leancd-e2e";

/// Holds the ephemeral cluster. Constructed once per test process via
/// [`Fixture::get`]; dropped (and the cluster deleted) at process exit.
pub struct Fixture {
    cluster: &'static str,
    forgejo: Forgejo,
}

impl Fixture {
    /// The shared, lazily-initialized fixture.
    pub fn get() -> &'static Fixture {
        static FIXTURE: OnceLock<Fixture> = OnceLock::new();
        FIXTURE.get_or_init(Fixture::start)
    }

    /// The ready in-cluster Forgejo.
    pub fn forgejo(&self) -> &Forgejo {
        &self.forgejo
    }

    fn start() -> Fixture {
        check_prereqs();
        // Fresh cluster: delete any leftover, then create.
        let _ = run(&["kind", "delete", "cluster", "--name", CLUSTER_NAME]);
        run(&["kind", "create", "cluster", "--name", CLUSTER_NAME]);
        // Build the Lean CD image and load it into the kind node.
        run(&["docker", "build", "-t", "leancd:latest", "."]);
        // Sanity: the image is the real binary, not the throwaway `fn main(){}`
        // dummy that BuildKit + Cargo's mtime fingerprint can ship (BUG 1).
        let version = run(&[
            "docker",
            "run",
            "--rm",
            "--entrypoint",
            "leancd",
            "leancd:latest",
            "--version",
        ]);
        assert!(
            version.trim().starts_with("leancd"),
            "built image is not the real leancd binary (got: {version})",
        );
        run(&[
            "kind",
            "load",
            "docker-image",
            "leancd:latest",
            "--name",
            CLUSTER_NAME,
        ]);
        // Sanity: the cluster is up and has a ready node.
        let nodes = run(&["kubectl", "get", "nodes", "--no-headers"]);
        assert!(
            nodes.contains("Ready"),
            "kind cluster has no Ready node: {nodes}"
        );
        // Deploy in-cluster Forgejo.
        let forgejo = Forgejo::deploy();
        // A repo the dormant controller points at (auto-initialized on `main`
        // so its first reconcile clones cleanly without touching scenario state).
        forgejo.create_repo("controller-idle", true);
        // Deploy in-cluster Lean CD + OTel collector (metrics over OTLP/HTTP).
        run(&["kubectl", "apply", "-f", "tests/leancd.yaml"]);
        run(&[
            "kubectl",
            "wait",
            "-n",
            "leancd",
            "--for=condition=Available",
            "deploy/otel-collector",
            "--timeout=240s",
        ]);
        run(&[
            "kubectl",
            "wait",
            "-n",
            "leancd",
            "--for=condition=Available",
            "deploy/leancd",
            "--timeout=240s",
        ]);
        Fixture {
            cluster: CLUSTER_NAME,
            forgejo,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = run(&["kind", "delete", "cluster", "--name", self.cluster]);
    }
}

/// Assert that the tools the harness shells out to are on PATH.
fn check_prereqs() {
    let checks: &[(&str, &[&str])] = &[
        ("docker", &["--version"]),
        ("helm", &["version"]),
        ("kind", &["version"]),
        ("kubectl", &["version", "--client"]),
        ("git", &["--version"]),
        ("curl", &["--version"]),
    ];
    for (tool, args) in checks {
        let ok = Command::new(tool)
            .args(*args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            ok,
            "make e2e requires: docker, helm, kind, kubectl, git, curl. \
             Missing or failing: {tool}"
        );
    }
}

/// Run a command (in the test process cwd, i.e. the repo root), asserting
/// success and returning trimmed stdout. The command line is echoed to stderr
/// so progress is visible under `--nocapture`; on failure stderr is in the panic.
pub fn run(args: &[&str]) -> String {
    eprintln!(">> {}", args.join(" "));
    let (prog, rest) = args.split_first().expect("non-empty args");
    let output = Command::new(prog)
        .args(rest)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {prog}: {e}"));
    if !output.status.success() {
        panic!(
            "{:?} failed (exit {:?}): {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
