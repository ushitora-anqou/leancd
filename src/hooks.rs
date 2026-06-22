//! Helm hook execution with Argo CD-equivalent semantics.
//!
//! A resource carrying a `helm.sh/hook` annotation is a *hook*: it is excluded
//! from the normal apply/prune of "main" resources and instead runs in a sync
//! phase around the main apply. Phase mapping follows Argo CD (install and
//! upgrade are indistinguishable in Lean CD's single apply, so they collapse):
//!
//! | `helm.sh/hook`               | phase                       |
//! |------------------------------|-----------------------------|
//! | `pre-install` / `pre-upgrade`  | PreSync (before main apply)   |
//! | `post-install` / `post-upgrade`| PostSync (after main apply)   |
//! | `pre-delete`                 | PreDelete (full teardown)    |
//! | `post-delete`                | PostDelete (full teardown)   |
//!
//! A hook whose annotation contains *only* unsupported types (e.g. `test`,
//! `rollback`, `crd-install`) maps to no phase and is ignored — never applied.
//! A hook with a mix of supported and unsupported types runs in the supported
//! phase(s) only. Within a phase, hooks run in ascending `helm.sh/hook-weight`
//! order (ties broken by name), matching Helm/Argo CD. Each hook is applied,
//! then — for Job/Pod hooks — awaited to completion; the outcome drives the
//! `helm.sh/hook-delete-policy`.

use kube::client::Client;
use kube::core::ApiResource;
use kube::discovery::Scope;
use serde_json::Value;

use crate::config::Config;
use crate::kube_util;
use crate::lock;
use crate::manifest::{self, RawManifest};
use crate::prune::ResourceKey;

/// Polling interval while awaiting a hook resource's completion.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Annotation marking a resource as a Helm hook and naming its type(s).
pub const HOOK_ANNOTATION: &str = "helm.sh/hook";
/// Hook execution ordering weight (integer; default 0).
pub const HOOK_WEIGHT_ANNOTATION: &str = "helm.sh/hook-weight";
/// When a hook resource is deleted.
pub const HOOK_DELETE_POLICY_ANNOTATION: &str = "helm.sh/hook-delete-policy";
/// Resource retention policy; `keep` exempts a resource from pruning.
pub const RESOURCE_POLICY_ANNOTATION: &str = "helm.sh/resource-policy";
/// The value that exempts a resource from deletion.
pub const RESOURCE_POLICY_KEEP: &str = "keep";

/// A sync phase in which hooks execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    PreSync,
    PostSync,
    PreDelete,
    PostDelete,
}

/// When a hook resource is deleted, relative to its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDeletePolicy {
    BeforeHookCreation,
    HookSucceeded,
    HookFailed,
}

/// Terminal status of a hook resource, read from its live `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Running,
    Succeeded,
    Failed,
}

/// A hook ready to run in a phase: the manifest plus parsed weight/policy.
#[derive(Debug, Clone)]
pub struct HookInfo {
    pub manifest: RawManifest,
    pub weight: i64,
    pub delete_policies: Vec<HookDeletePolicy>,
}

impl HookInfo {
    /// Stable identity of the hook resource (for apply/wait/delete by name).
    pub fn key(&self) -> ResourceKey {
        ResourceKey::from_manifest(&self.manifest)
    }

    fn has_policy(&self, p: HookDeletePolicy) -> bool {
        self.delete_policies.contains(&p)
    }
}

/// Manifests partitioned by hook phase. `main` holds non-hook resources; hooks
/// with only unsupported types are dropped (present nowhere).
#[derive(Debug, Default)]
pub struct Classified {
    pub pre: Vec<HookInfo>,
    pub main: Vec<RawManifest>,
    pub post: Vec<HookInfo>,
    pub pre_delete: Vec<HookInfo>,
    pub post_delete: Vec<HookInfo>,
}

/// Map a raw `helm.sh/hook` CSV value to its phase(s). Unsupported tokens are
/// dropped; duplicate phases collapse to one. Tokens are trimmed and
/// lower-cased so `Pre-Install` matches `pre-install`.
pub fn hook_phases(raw: &str) -> Vec<HookPhase> {
    let mut out: Vec<HookPhase> = Vec::new();
    for tok in raw.split(',') {
        let phase = match tok.trim().to_ascii_lowercase().as_str() {
            "pre-install" | "pre-upgrade" => HookPhase::PreSync,
            "post-install" | "post-upgrade" => HookPhase::PostSync,
            "pre-delete" => HookPhase::PreDelete,
            "post-delete" => HookPhase::PostDelete,
            _ => continue,
        };
        if !out.contains(&phase) {
            out.push(phase);
        }
    }
    out
}

/// Partition parsed manifests into main resources and per-phase hooks. A
/// resource with a `helm.sh/hook` annotation but no recognized phase is ignored
/// (not applied). Hooks are not sorted here; see [`sort_by_weight`].
pub fn classify(manifests: Vec<RawManifest>) -> Classified {
    let mut c = Classified::default();
    for m in manifests {
        let raw = manifest::annotation(&m, HOOK_ANNOTATION);
        let phases = match &raw {
            Some(r) => hook_phases(r),
            None => Vec::new(),
        };
        if phases.is_empty() {
            if raw.is_some() {
                // Hook with only unsupported types: ignore entirely.
                continue;
            }
            c.main.push(m);
            continue;
        }
        let weight = hook_weight(&m);
        let policies = parse_delete_policies(&m);
        for phase in phases {
            let info = HookInfo {
                manifest: m.clone(),
                weight,
                delete_policies: policies.clone(),
            };
            match phase {
                HookPhase::PreSync => c.pre.push(info),
                HookPhase::PostSync => c.post.push(info),
                HookPhase::PreDelete => c.pre_delete.push(info),
                HookPhase::PostDelete => c.post_delete.push(info),
            }
        }
    }
    c
}

/// Parse `helm.sh/hook-weight` as an integer; missing or unparseable → 0.
/// Matches Helm's `calculateHookWeight`.
pub fn hook_weight(m: &RawManifest) -> i64 {
    manifest::annotation(m, HOOK_WEIGHT_ANNOTATION)
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Parse `helm.sh/hook-delete-policy`. Missing/empty/all-unrecognized →
/// `[BeforeHookCreation]` (the Helm default); otherwise exactly the listed
/// policies. Multiple comma-separated values are supported.
pub fn parse_delete_policies(m: &RawManifest) -> Vec<HookDeletePolicy> {
    let raw = match manifest::annotation(m, HOOK_DELETE_POLICY_ANNOTATION) {
        Some(s) => s,
        None => return vec![HookDeletePolicy::BeforeHookCreation],
    };
    let parsed: Vec<HookDeletePolicy> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter_map(|s| match s.as_str() {
            "before-hook-creation" => Some(HookDeletePolicy::BeforeHookCreation),
            "hook-succeeded" => Some(HookDeletePolicy::HookSucceeded),
            "hook-failed" => Some(HookDeletePolicy::HookFailed),
            _ => None,
        })
        .collect();
    if parsed.is_empty() {
        vec![HookDeletePolicy::BeforeHookCreation]
    } else {
        parsed
    }
}

/// Sort hooks for a phase by ascending weight, then ascending name, matching
/// Helm's `hookByWeight`.
pub fn sort_by_weight(hooks: &mut [HookInfo]) {
    hooks.sort_by(|a, b| {
        a.weight
            .cmp(&b.weight)
            .then_with(|| a.manifest.name.cmp(&b.manifest.name))
    });
}

/// Whether a resource kind is awaited to completion (Job/Pod) rather than
/// treated as complete on apply.
pub fn is_waitable(group: &str, kind: &str) -> bool {
    (group == "batch" && kind == "Job") || (group.is_empty() && kind == "Pod")
}

/// Read a hook resource's terminal status from its live `status` value. Non-
/// waitable kinds are considered succeeded immediately. A missing/null status
/// is `Running` for waitable kinds.
pub fn hook_completion(group: &str, kind: &str, status: Option<&Value>) -> Completion {
    let status = match status {
        Some(s) if !s.is_null() => s,
        _ => {
            return if is_waitable(group, kind) {
                Completion::Running
            } else {
                Completion::Succeeded
            };
        }
    };
    if group == "batch" && kind == "Job" {
        if let Some(conds) = status.get("conditions").and_then(|c| c.as_array()) {
            for c in conds {
                let ty = c.get("type").and_then(|v| v.as_str());
                let st = c.get("status").and_then(|v| v.as_str());
                match (ty, st) {
                    (Some("Complete"), Some("True")) => return Completion::Succeeded,
                    (Some("Failed"), Some("True")) => return Completion::Failed,
                    _ => {}
                }
            }
        }
        return Completion::Running;
    }
    if group.is_empty() && kind == "Pod" {
        return match status.get("phase").and_then(|v| v.as_str()) {
            Some("Succeeded") => Completion::Succeeded,
            Some("Failed") => Completion::Failed,
            _ => Completion::Running,
        };
    }
    Completion::Succeeded
}

/// Outcome of running one phase: how many hooks were started, and the ones that
/// failed (apply error, non-zero completion, or timeout). Empty `failures` means
/// the phase succeeded; `attempted - failures.len()` succeeded.
#[derive(Debug, Default)]
pub struct PhaseOutcome {
    pub attempted: usize,
    pub failures: Vec<(ResourceKey, String)>,
}

/// Run a phase's hooks in ascending weight order. Each hook is resolved,
/// optionally pre-deleted (`before-hook-creation`), applied, awaited to
/// completion (Job/Pod only), and deleted per `hook-delete-policy`. The first
/// failing hook stops the remaining hooks in the phase; its failure is
/// recorded. Per-hook problems (discovery, apply, completion, timeout) are all
/// captured as failures rather than aborting via `Err`, so the caller decides
/// whether a failed phase is fatal (PreSync/PreDelete) or merely recorded
/// (PostSync/PostDelete).
pub async fn run_phase(
    client: &Client,
    cfg: &Config,
    hooks: &[HookInfo],
    phase: HookPhase,
    lease: Option<&lock::LeaseGuard>,
) -> PhaseOutcome {
    let mut ordered: Vec<HookInfo> = hooks.to_vec();
    sort_by_weight(&mut ordered);
    let mut attempted = 0usize;
    let mut failures = Vec::new();
    for info in ordered {
        attempted += 1;
        tracing::info!(
            phase = ?phase,
            name = %info.manifest.name,
            kind = %info.manifest.kind,
            weight = info.weight,
            "running helm hook"
        );
        if let Err((key, reason)) = run_one(client, cfg, &info, lease).await {
            tracing::warn!(phase = ?phase, key = ?key, reason = %reason, "helm hook failed");
            failures.push((key, reason));
            break;
        }
    }
    PhaseOutcome {
        attempted,
        failures,
    }
}

/// Run a single hook end-to-end. `Err` carries the identity and a reason.
async fn run_one(
    client: &Client,
    cfg: &Config,
    info: &HookInfo,
    lease: Option<&lock::LeaseGuard>,
) -> std::result::Result<(), (ResourceKey, String)> {
    let key = info.key();
    let m = &info.manifest;

    let (ar, caps) = kube_util::resolve(client, &m.group, &m.version, &m.kind)
        .await
        .map_err(|e| (key.clone(), format!("api discovery failed: {e}")))?;

    // before-hook-creation: remove any prior instance so the hook runs fresh.
    // A missing object (first run) is expected and ignored.
    if info.has_policy(HookDeletePolicy::BeforeHookCreation) {
        let _ = kube_util::delete(
            client,
            &ar,
            &caps.scope,
            m.namespace.as_deref(),
            &cfg.namespace,
            &m.name,
        )
        .await;
    }

    kube_util::apply(
        client,
        &ar,
        &caps.scope,
        &cfg.namespace,
        &m.data,
        &cfg.field_manager,
    )
    .await
    .map_err(|e| (key.clone(), format!("apply failed: {e}")))?;

    // Await completion only for kinds with a defined terminal state.
    let completion = if is_waitable(&m.group, &m.kind) {
        wait_for_completion(client, &ar, &caps.scope, cfg, m, lease).await
    } else {
        Completion::Succeeded
    };

    // Honor hook-succeeded / hook-failed deletion.
    let delete_after = match completion {
        Completion::Succeeded => info.has_policy(HookDeletePolicy::HookSucceeded),
        Completion::Failed => info.has_policy(HookDeletePolicy::HookFailed),
        Completion::Running => false,
    };
    if delete_after {
        let _ = kube_util::delete(
            client,
            &ar,
            &caps.scope,
            m.namespace.as_deref(),
            &cfg.namespace,
            &m.name,
        )
        .await;
    }

    if completion == Completion::Failed {
        return Err((key.clone(), "hook completed with failure".to_string()));
    }
    Ok(())
}

/// Poll a waitable hook resource until it reaches a terminal state or the
/// configured timeout elapses. A timeout or a vanished resource is treated as
/// `Failed`; transient poll errors are retried until the deadline.
async fn wait_for_completion(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    cfg: &Config,
    m: &RawManifest,
    lease: Option<&lock::LeaseGuard>,
) -> Completion {
    let deadline = tokio::time::Instant::now() + cfg.hook_timeout;
    loop {
        // Refresh the reconcile lease each poll so a long hook wait does not
        // let the lease go stale and get reclaimed mid-pass. Best-effort: a
        // lost/failed renew never fails the hook (the PID-scoped work_dir and
        // next-pass convergence still hold).
        if let Some(g) = lease {
            match lock::renew(client, cfg, g).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("reconcile lease lost while awaiting hook; continuing")
                }
                Err(e) => tracing::warn!(error = %e, "lease renew failed while awaiting hook"),
            }
        }
        match kube_util::get(
            client,
            ar,
            scope,
            m.namespace.as_deref(),
            &cfg.namespace,
            &m.name,
        )
        .await
        {
            Ok(Some(obj)) => {
                let status = obj.data.get("status");
                match hook_completion(&m.group, &m.kind, status) {
                    Completion::Running => {}
                    terminal => return terminal,
                }
            }
            Ok(None) => {
                tracing::warn!(
                    name = %m.name,
                    kind = %m.kind,
                    "hook resource disappeared while awaiting completion"
                );
                return Completion::Failed;
            }
            Err(e) => {
                tracing::warn!(error = %e, name = %m.name, "hook status poll failed; retrying");
            }
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                name = %m.name,
                kind = %m.kind,
                timeout_secs = cfg.hook_timeout.as_secs(),
                "hook did not complete within timeout; treating as failed"
            );
            return Completion::Failed;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a manifest with the given identity and annotations (deterministic
    /// `apiVersion` from the group).
    fn manifest_with(name: &str, group: &str, kind: &str, annos: &[(&str, &str)]) -> RawManifest {
        let mut meta = serde_json::Map::new();
        meta.insert("name".into(), json!(name));
        if !annos.is_empty() {
            let a: serde_json::Map<String, serde_json::Value> = annos
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            meta.insert("annotations".into(), Value::Object(a));
        }
        let api_version = if group.is_empty() {
            "v1".to_string()
        } else {
            format!("{group}/v1")
        };
        RawManifest {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: None,
            data: json!({
                "apiVersion": api_version,
                "kind": kind,
                "metadata": Value::Object(meta),
            }),
        }
    }

    // --- hook_phases: CSV -> phases ---

    #[test]
    fn hook_phases_supported_mapping() {
        assert_eq!(hook_phases("pre-install"), vec![HookPhase::PreSync]);
        assert_eq!(hook_phases("pre-upgrade"), vec![HookPhase::PreSync]);
        assert_eq!(hook_phases("post-install"), vec![HookPhase::PostSync]);
        assert_eq!(hook_phases("post-upgrade"), vec![HookPhase::PostSync]);
        assert_eq!(hook_phases("pre-delete"), vec![HookPhase::PreDelete]);
        assert_eq!(hook_phases("post-delete"), vec![HookPhase::PostDelete]);
    }

    #[test]
    fn hook_phases_unsupported_only_is_empty() {
        assert!(hook_phases("test").is_empty());
        assert!(hook_phases("rollback").is_empty());
        assert!(hook_phases("crd-install").is_empty());
        assert!(hook_phases("garbage").is_empty());
        assert!(hook_phases("").is_empty());
    }

    #[test]
    fn hook_phases_dedup_and_partial_support() {
        assert_eq!(
            hook_phases("pre-install,pre-upgrade"),
            vec![HookPhase::PreSync]
        );
        assert_eq!(hook_phases("pre-install,test"), vec![HookPhase::PreSync]);
        assert_eq!(
            hook_phases("pre-install,post-install"),
            vec![HookPhase::PreSync, HookPhase::PostSync]
        );
    }

    #[test]
    fn hook_phases_trims_and_lowercases() {
        assert_eq!(hook_phases(" Pre-Install "), vec![HookPhase::PreSync]);
    }

    // --- classify ---

    #[test]
    fn classify_no_hook_goes_to_main() {
        let c = classify(vec![manifest_with("a", "", "ConfigMap", &[])]);
        assert_eq!(c.main.len(), 1);
        assert!(c.pre.is_empty() && c.post.is_empty());
        assert!(c.pre_delete.is_empty() && c.post_delete.is_empty());
    }

    #[test]
    fn classify_pre_install_and_upgrade_are_presync() {
        for hook in ["pre-install", "pre-upgrade"] {
            let c = classify(vec![manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook", hook)],
            )]);
            assert_eq!(c.pre.len(), 1, "{hook}");
            assert!(c.post.is_empty());
            assert!(c.main.is_empty(), "{hook} must not be a main resource");
        }
    }

    #[test]
    fn classify_post_install_and_upgrade_are_postsync() {
        for hook in ["post-install", "post-upgrade"] {
            let c = classify(vec![manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook", hook)],
            )]);
            assert_eq!(c.post.len(), 1, "{hook}");
            assert!(c.pre.is_empty());
        }
    }

    #[test]
    fn classify_delete_hooks() {
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "pre-delete")],
        )]);
        assert_eq!(c.pre_delete.len(), 1);
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "post-delete")],
        )]);
        assert_eq!(c.post_delete.len(), 1);
    }

    #[test]
    fn classify_unsupported_only_hook_is_ignored() {
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "test")],
        )]);
        assert!(c.main.is_empty());
        assert!(c.pre.is_empty());
        assert!(c.post.is_empty());
        assert!(c.pre_delete.is_empty());
        assert!(c.post_delete.is_empty());
    }

    #[test]
    fn classify_mixed_supported_unsupported_keeps_supported() {
        // pre-install,test -> PreSync only (test dropped). [derived decision A]
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "pre-install,test")],
        )]);
        assert_eq!(c.pre.len(), 1);
        assert!(c.main.is_empty());
    }

    #[test]
    fn classify_multi_phase_hook_runs_in_each() {
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "pre-install,post-install")],
        )]);
        assert_eq!(c.pre.len(), 1);
        assert_eq!(c.post.len(), 1);
    }

    #[test]
    fn classify_duplicate_phase_collapses() {
        let c = classify(vec![manifest_with(
            "h",
            "batch",
            "Job",
            &[("helm.sh/hook", "pre-install,pre-upgrade")],
        )]);
        assert_eq!(c.pre.len(), 1);
    }

    #[test]
    fn classify_carries_weight_and_policy() {
        let m = manifest_with(
            "h",
            "batch",
            "Job",
            &[
                ("helm.sh/hook", "pre-install"),
                ("helm.sh/hook-weight", "-5"),
                ("helm.sh/hook-delete-policy", "hook-succeeded"),
            ],
        );
        let c = classify(vec![m]);
        assert_eq!(c.pre[0].weight, -5);
        assert_eq!(
            c.pre[0].delete_policies,
            vec![HookDeletePolicy::HookSucceeded]
        );
        assert_eq!(c.pre[0].manifest.name, "h");
    }

    // --- hook_weight ---

    #[test]
    fn weight_missing_is_zero() {
        assert_eq!(hook_weight(&manifest_with("h", "batch", "Job", &[])), 0);
    }

    #[test]
    fn weight_parsed_including_negative() {
        assert_eq!(
            hook_weight(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-weight", "7")]
            )),
            7
        );
        assert_eq!(
            hook_weight(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-weight", "-3")]
            )),
            -3
        );
    }

    #[test]
    fn weight_invalid_is_zero() {
        assert_eq!(
            hook_weight(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-weight", "nope")]
            )),
            0
        );
    }

    // --- parse_delete_policies ---

    #[test]
    fn delete_policy_missing_defaults_before_hook_creation() {
        assert_eq!(
            parse_delete_policies(&manifest_with("h", "batch", "Job", &[])),
            vec![HookDeletePolicy::BeforeHookCreation]
        );
    }

    #[test]
    fn delete_policy_single_value() {
        assert_eq!(
            parse_delete_policies(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-delete-policy", "hook-succeeded")]
            )),
            vec![HookDeletePolicy::HookSucceeded]
        );
    }

    #[test]
    fn delete_policy_multiple_values() {
        assert_eq!(
            parse_delete_policies(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-delete-policy", "hook-succeeded, hook-failed")]
            )),
            vec![
                HookDeletePolicy::HookSucceeded,
                HookDeletePolicy::HookFailed
            ]
        );
    }

    #[test]
    fn delete_policy_unrecognized_defaults() {
        assert_eq!(
            parse_delete_policies(&manifest_with(
                "h",
                "batch",
                "Job",
                &[("helm.sh/hook-delete-policy", "garbage")]
            )),
            vec![HookDeletePolicy::BeforeHookCreation]
        );
    }

    // --- sort_by_weight ---

    #[test]
    fn sort_by_weight_ascending_then_name() {
        let mk = |name: &str, weight: i64| HookInfo {
            manifest: manifest_with(name, "batch", "Job", &[]),
            weight,
            delete_policies: vec![],
        };
        let mut hooks = vec![mk("b", 0), mk("a", 5), mk("c", -2), mk("a", 0)];
        sort_by_weight(&mut hooks);
        let names: Vec<&str> = hooks.iter().map(|h| h.manifest.name.as_str()).collect();
        // (weight,name): c(-2,c), a(0,a), b(0,b), a(5,a)
        assert_eq!(names, vec!["c", "a", "b", "a"]);
    }

    // --- is_waitable / hook_completion ---

    #[test]
    fn is_waitable_job_and_pod_only() {
        assert!(is_waitable("batch", "Job"));
        assert!(is_waitable("", "Pod"));
        assert!(!is_waitable("", "ConfigMap"));
        assert!(!is_waitable("batch", "CronJob"));
    }

    #[test]
    fn job_complete_is_succeeded() {
        let status = json!({"conditions":[{"type":"Complete","status":"True"}]});
        assert_eq!(
            hook_completion("batch", "Job", Some(&status)),
            Completion::Succeeded
        );
    }

    #[test]
    fn job_failed_condition_is_failed() {
        let status = json!({"conditions":[{"type":"Failed","status":"True"}]});
        assert_eq!(
            hook_completion("batch", "Job", Some(&status)),
            Completion::Failed
        );
    }

    #[test]
    fn job_running_without_terminal_condition() {
        let status = json!({"conditions":[{"type":"Running","status":"True"}]});
        assert_eq!(
            hook_completion("batch", "Job", Some(&status)),
            Completion::Running
        );
        assert_eq!(hook_completion("batch", "Job", None), Completion::Running);
    }

    #[test]
    fn job_complete_false_still_running() {
        let status = json!({"conditions":[{"type":"Complete","status":"False"}]});
        assert_eq!(
            hook_completion("batch", "Job", Some(&status)),
            Completion::Running
        );
    }

    #[test]
    fn pod_phase_mapping() {
        assert_eq!(
            hook_completion("", "Pod", Some(&json!({"phase":"Succeeded"}))),
            Completion::Succeeded
        );
        assert_eq!(
            hook_completion("", "Pod", Some(&json!({"phase":"Failed"}))),
            Completion::Failed
        );
        assert_eq!(
            hook_completion("", "Pod", Some(&json!({"phase":"Pending"}))),
            Completion::Running
        );
    }

    #[test]
    fn non_waitable_kind_is_immediate_succeeded() {
        assert_eq!(
            hook_completion("", "ConfigMap", None),
            Completion::Succeeded
        );
        assert_eq!(
            hook_completion("", "ConfigMap", Some(&json!({}))),
            Completion::Succeeded
        );
    }

    #[test]
    fn hook_info_key_preserves_identity() {
        let m = manifest_with("h", "batch", "Job", &[("helm.sh/hook", "pre-install")]);
        let info = classify(vec![m]).pre.pop().unwrap();
        let key = info.key();
        assert_eq!(key.group, "batch");
        assert_eq!(key.kind, "Job");
        assert_eq!(key.name, "h");
    }
}
