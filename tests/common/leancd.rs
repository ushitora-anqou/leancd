//! Run Lean CD subcommands inside the in-cluster Deployment, and launch
//! short-lived controller Jobs for scenarios that need active polling.

use std::process::Command;

use crate::common::kubectl;

/// Outcome of running a Lean CD subcommand via `kubectl exec`. `exit_code` is
/// the process exit status (or -1 when kubectl did not report one); `leancd
/// health` uses distinct codes (0=fresh, 1=never, 2=stale, 3=failing).
pub struct RunResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run `leancd sync [args]` in the Lean CD Deployment.
pub fn sync(args: &[String]) -> RunResult {
    exec_leancd("sync", args)
}

/// Launch `leancd sync [args]` on a background thread and return immediately.
/// Used by scenarios that must act on a resource *while* the sync is in flight
/// (e.g. parking a finalizer on a hook Pod before the hook completes and is
/// deleted). Block on the result with [`SyncHandle::join`].
pub fn sync_handle(args: Vec<String>) -> SyncHandle {
    SyncHandle {
        handle: Some(std::thread::spawn(move || exec_leancd("sync", &args))),
    }
}

/// A background `leancd sync`. [`SyncHandle::join`] reaps the result; if dropped
/// unjoined, `Drop` joins the thread so a sync never outlives its scenario.
pub struct SyncHandle {
    handle: Option<std::thread::JoinHandle<RunResult>>,
}

impl SyncHandle {
    /// Block until the background sync finishes and return its result.
    pub fn join(mut self) -> RunResult {
        self.handle
            .take()
            .expect("sync handle already joined")
            .join()
            .unwrap_or_else(|e| panic!("background sync thread panicked: {e:?}"))
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Run `leancd status [args]` in the Lean CD Deployment.
pub fn status(args: &[String]) -> RunResult {
    exec_leancd("status", args)
}

/// Run `leancd health [args]` in the Lean CD Deployment and return its exit
/// code (0=fresh, 1=never, 2=stale, 3=failing). For liveness/readiness probe
/// assertions; the Deployment's env (`LEANCD_NAMESPACE`) is inherited.
pub fn health(args: &[String]) -> RunResult {
    exec_leancd("health", args)
}

fn exec_leancd(sub: &str, args: &[String]) -> RunResult {
    let mut argv: Vec<String> = vec![
        "exec".into(),
        "-n".into(),
        "leancd".into(),
        "deploy/leancd".into(),
        "--".into(),
        "leancd".into(),
        sub.into(),
    ];
    argv.extend(args.iter().cloned());
    eprintln!(">> kubectl {}", argv.join(" "));
    let strs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let out = Command::new("kubectl")
        .args(&strs)
        .output()
        .unwrap_or_else(|e| panic!("kubectl exec leancd {sub}: {e}"));
    RunResult {
        success: out.status.success(),
        exit_code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

/// Launch a short-poll (2s) Lean CD controller as a Job, so a scenario can
/// observe automatic reconciliation. `args` are the common flags (the same
/// `sync_args` used by `sync`). The Job is deleted when the handle drops.
pub fn controller(name: &str, args: Vec<String>) -> ControllerHandle {
    controller_with_opts(name, args, "2s")
}

/// Like [`controller`] but with a configurable poll interval. Used by watch
/// scenarios that pin a long poll so a fast self-heal can only come from the
/// watch trigger, not from the periodic loop.
pub fn controller_with_opts(
    name: &str,
    mut args: Vec<String>,
    poll_interval: &str,
) -> ControllerHandle {
    let mut full = vec!["controller".to_string()];
    full.append(&mut args);
    full.push("--poll-interval".into());
    full.push(poll_interval.into());
    let args_yaml: String = full
        .iter()
        .map(|a| {
            format!(
                "            - \"{}\"",
                a.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let job = format!(
        "apiVersion: batch/v1\n\
         kind: Job\n\
         metadata:\n  name: {name}\n  namespace: leancd\n\
         spec:\n  backoffLimit: 0\n  template:\n    spec:\n      \
         terminationGracePeriodSeconds: 30\n      serviceAccountName: leancd\n      restartPolicy: Never\n      \
         containers:\n        - name: leancd\n          image: leancd:latest\n          \
         imagePullPolicy: IfNotPresent\n          args:\n{args_yaml}\n          \
         envFrom:\n            - secretRef:\n                name: leancd-git-credentials\n"
    );
    kubectl::apply_stdin(&job);
    eprintln!(">> started controller job {name} (poll-interval {poll_interval})");
    ControllerHandle {
        name: name.to_string(),
    }
}

/// A running controller Job; deleted on drop.
pub struct ControllerHandle {
    name: String,
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        eprintln!(">> deleting controller job {}", self.name);
        let _ = Command::new("kubectl")
            .args([
                "delete",
                "job",
                "-n",
                "leancd",
                &self.name,
                "--ignore-not-found=true",
            ])
            .output();
    }
}

/// Run a one-shot `leancd sync` over the SSH transport: materialise the given
/// private key into a `GIT_SSH_KEY` Secret, launch a sync Job that envFroms it,
/// wait for completion, and return the Pod's logs. The Job and Secret are
/// deleted afterwards.
pub fn sync_ssh(name: &str, ssh_key: &str, args: &[String]) -> RunResult {
    let key_block: String = ssh_key
        .trim_end()
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let secret = format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {name}-key\n  namespace: leancd\n\
         type: Opaque\nstringData:\n  GIT_SSH_KEY: |\n{key_block}\n"
    );
    kubectl::apply_stdin(&secret);

    let mut full = vec!["sync".to_string()];
    full.extend(args.iter().cloned());
    let args_yaml: String = full
        .iter()
        .map(|a| {
            format!(
                "            - \"{}\"",
                a.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let job = format!(
        "apiVersion: batch/v1\nkind: Job\nmetadata:\n  name: {name}\n  namespace: leancd\n\
         spec:\n  backoffLimit: 0\n  template:\n    spec:\n      \
         terminationGracePeriodSeconds: 30\n      serviceAccountName: leancd\n      restartPolicy: Never\n      \
         containers:\n        - name: leancd\n          image: leancd:latest\n          \
         imagePullPolicy: IfNotPresent\n          args:\n{args_yaml}\n          \
         envFrom:\n            - secretRef:\n                name: {name}-key\n"
    );
    kubectl::apply_stdin(&job);
    eprintln!(">> started ssh sync job {name}");

    let ok = crate::common::wait::wait_for(
        || job_succeeded(name),
        std::time::Duration::from_secs(90),
        std::time::Duration::from_millis(500),
    );
    let logs = Command::new("kubectl")
        .args(["logs", "-n", "leancd", &format!("job/{name}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let _ = Command::new("kubectl")
        .args([
            "delete",
            "job",
            "-n",
            "leancd",
            name,
            "--ignore-not-found=true",
        ])
        .output();
    let _ = Command::new("kubectl")
        .args([
            "delete",
            "secret",
            "-n",
            "leancd",
            &format!("{name}-key"),
            "--ignore-not-found=true",
        ])
        .output();

    RunResult {
        success: ok,
        // The Job container's exit code is not captured separately here; derive
        // a conventional 0/1 from succeeded/failed. (Not used by `health`.)
        exit_code: if ok { 0 } else { 1 },
        stdout: logs,
        stderr: String::new(),
    }
}

fn job_succeeded(name: &str) -> bool {
    let out = Command::new("kubectl")
        .args([
            "get",
            "job",
            "-n",
            "leancd",
            name,
            "-o",
            "jsonpath={.status.succeeded}",
        ])
        .output()
        .unwrap_or_else(|e| panic!("kubectl get job: {e}"));
    String::from_utf8_lossy(&out.stdout).trim() == "1"
}
