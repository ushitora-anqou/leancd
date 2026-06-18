//! Foreground-cascade deletion proof helpers.
//!
//! leancd deletes every resource with `DeleteParams::foreground()`. Foreground
//! cascade — unlike background — stamps a `foregroundDeletion` finalizer on the
//! owner and removes its dependents first. To prove this deterministically
//! without racing the kind garbage collector, a scenario parks a custom *stall*
//! finalizer on a dependent (a ConfigMap with an explicit ownerReference, or a
//! Pod owned by a Job). While the dependent is stalled, the owner lingers with
//! `foregroundDeletion` set, which the scenario asserts on. Background deletion
//! never sets that finalizer, so a regression fails the test.
//!
//! These are thin `kubectl patch`/`kubectl get` wrappers, mirroring
//! [`crate::common::kubectl`]: no new dependencies, test-binary only (the RSS
//! budget concerns the release binary, not the test crate).

use std::process::Command;
use std::time::Duration;

use crate::common::{kubectl, wait};

/// The finalizer the kube apiserver stamps on an owner during foreground
/// deletion. Its presence is the proof that leancd deleted in the foreground.
pub const FOREGROUND_FINALIZER: &str = "foregroundDeletion";

/// A throwaway finalizer parked on a dependent to stall cascade deletion so the
/// owner's `foregroundDeletion` finalizer stays observable. The name must be
/// fully qualified (`<domain>/<name>`) — Kubernetes rejects bare finalizer
/// names that are not standard (like `foregroundDeletion`).
pub const STALL_FINALIZER: &str = "leancd.dev/e2e-stall";

/// Start a `kubectl patch <kind>/<name> [-n <ns>]` command.
fn patch_cmd(ns: &str, kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("kubectl");
    cmd.arg("patch").arg(kind).arg(name);
    if !ns.is_empty() {
        cmd.args(["-n", ns]);
    }
    cmd
}

/// Add [`STALL_FINALIZER`] to a resource, preserving any existing finalizers.
/// Uses a merge patch of the whole `metadata.finalizers` array: a json-patch
/// `add` to `/metadata/finalizers/-` fails when finalizers is unset (the common
/// case for a fresh Pod/ConfigMap). A missing resource here is a real test
/// failure, so the patch status is asserted.
pub fn add_stall_finalizer(ns: &str, kind: &str, name: &str) {
    let obj = kubectl::get_json(ns, kind, name);
    let mut finalizers: Vec<String> = obj["metadata"]["finalizers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !finalizers.iter().any(|f| f == STALL_FINALIZER) {
        finalizers.push(STALL_FINALIZER.to_string());
    }
    let arr = finalizers
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(",");
    let patch = format!("{{\"metadata\":{{\"finalizers\":[{arr}]}}}}");
    let out = patch_cmd(ns, kind, name)
        .args(["--type=merge", "-p", &patch])
        .output()
        .unwrap_or_else(|e| panic!("kubectl patch add finalizer: {e}"));
    assert!(
        out.status.success(),
        "add stall finalizer {kind}/{name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Strip every finalizer from a resource via a merge patch (finalizers = []),
/// letting a stalled cascade complete. Used for test teardown.
pub fn remove_stall_finalizer(ns: &str, kind: &str, name: &str) {
    let out = patch_cmd(ns, kind, name)
        .args(["--type=merge", "-p", "{\"metadata\":{\"finalizers\":[]}}"])
        .output()
        .unwrap_or_else(|e| panic!("kubectl patch remove finalizer: {e}"));
    assert!(
        out.status.success(),
        "remove stall finalizer {kind}/{name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Resolve `kind/name`'s `.metadata.uid` via `kubectl get -o jsonpath`.
fn uid_of(ns: &str, kind: &str, name: &str) -> String {
    let mut cmd = Command::new("kubectl");
    cmd.arg("get").arg(kind).arg(name);
    if !ns.is_empty() {
        cmd.args(["-n", ns]);
    }
    cmd.args(["-o", "jsonpath={.metadata.uid}"]);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("kubectl get uid: {e}"));
    assert!(
        out.status.success(),
        "get uid {kind}/{name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Merge a complete `ownerReference` onto a dependent so the GC controller
/// treats it as a real dependent of `owner` for foreground cascade scheduling.
/// The reference carries the owner's UID plus `controller: true` and
/// `blockOwnerDeletion: true`; `owner_api_version` is the owner's GVK
/// (`v1` for a ConfigMap, `batch/v1` for a Job, …).
pub fn link_owner(
    ns: &str,
    dep_kind: &str,
    dep_name: &str,
    owner_kind: &str,
    owner_name: &str,
    owner_api_version: &str,
) {
    let uid = uid_of(ns, owner_kind, owner_name);
    let patch = format!(
        "{{\"metadata\":{{\"ownerReferences\":[{{\"apiVersion\":\"{owner_api_version}\",\"kind\":\"{owner_kind}\",\"name\":\"{owner_name}\",\"uid\":\"{uid}\",\"controller\":true,\"blockOwnerDeletion\":true}}]}}}}"
    );
    let out = patch_cmd(ns, dep_kind, dep_name)
        .args(["--type=merge", "-p", &patch])
        .output()
        .unwrap_or_else(|e| panic!("kubectl patch ownerReferences: {e}"));
    assert!(
        out.status.success(),
        "link_owner {dep_kind}/{dep_name} -> {owner_kind}/{owner_name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// True iff `kind/name` carries [`FOREGROUND_FINALIZER`]. Reads finalizers via
/// jsonpath so a vanished (404) resource returns `false` rather than panicking
/// — the owner may legitimately be gone if the stall window was missed.
pub fn has_foreground_finalizer(ns: &str, kind: &str, name: &str) -> bool {
    let mut cmd = Command::new("kubectl");
    cmd.arg("get").arg(kind).arg(name);
    if !ns.is_empty() {
        cmd.args(["-n", ns]);
    }
    cmd.args(["-o", "jsonpath={.metadata.finalizers}"]);
    let out = match cmd.output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{FOREGROUND_FINALIZER}\""))
}

/// Poll up to `timeout` for `kind/name` to gain the `foregroundDeletion`
/// finalizer. Returns true if observed; the caller asserts on the result.
pub fn wait_for_foreground(ns: &str, kind: &str, name: &str, timeout: Duration) -> bool {
    wait::wait_for(
        || has_foreground_finalizer(ns, kind, name),
        timeout,
        Duration::from_millis(300),
    )
}
