//! Resource health assessment, a port of Argo CD's built-in per-GVK health
//! checks (`gitops-engine/pkg/health/health.go` + `health_*.go`).
//!
//! Sync completion is unchanged — a successful apply still completes the sync.
//! Health is an *independent* signal: the worst health across managed
//! resources (with a built-in check) is persisted in the state ConfigMap,
//! exported as a metric, and shown by `leancd status`. Like Argo CD, we do
//! **not** descend `ownerReferences` — a Deployment's health reads its own
//! `.status` (which already aggregates its ReplicaSet/Pod state), so child
//! Pods are never listed directly. Health checks are pure functions over a
//! `&DynamicObject`; unsupported GVKs return `None` and are excluded from the
//! aggregate (Argo CD's `healthCheck == nil` behavior).

use kube::core::DynamicObject;
use serde_json::Value;

use crate::manifest::RawManifest;
use crate::state::HealthSummary;

/// A health state, mirroring Argo CD's `HealthStatusCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatusCode {
    Healthy,
    Progressing,
    Degraded,
    Suspended,
    Missing,
    Unknown,
}

impl HealthStatusCode {
    /// Stable string spelling, used for the persisted `worst` and the metric
    /// label. Matches Argo CD's spelling exactly.
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatusCode::Healthy => "Healthy",
            HealthStatusCode::Progressing => "Progressing",
            HealthStatusCode::Degraded => "Degraded",
            HealthStatusCode::Suspended => "Suspended",
            HealthStatusCode::Missing => "Missing",
            HealthStatusCode::Unknown => "Unknown",
        }
    }
}

/// A single resource's health (status + human-readable message).
#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub status: HealthStatusCode,
    pub message: String,
}

impl HealthStatus {
    fn new(status: HealthStatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn healthy() -> Self {
        Self::new(HealthStatusCode::Healthy, "")
    }
}

/// Worst-first ordering, identical to Argo CD's `healthOrder`
/// (`health.go:44-52`): index rises as health worsens.
const HEALTH_ORDER: [HealthStatusCode; 6] = [
    HealthStatusCode::Healthy,
    HealthStatusCode::Suspended,
    HealthStatusCode::Progressing,
    HealthStatusCode::Missing,
    HealthStatusCode::Degraded,
    HealthStatusCode::Unknown,
];

fn rank(code: HealthStatusCode) -> usize {
    HEALTH_ORDER
        .iter()
        .position(|&c| c == code)
        .unwrap_or(HEALTH_ORDER.len())
}

/// Argo CD's `IsWorse`: true when `new` is worse (higher in `HEALTH_ORDER`)
/// than `current`.
pub fn is_worse(current: HealthStatusCode, new: HealthStatusCode) -> bool {
    rank(new) > rank(current)
}

// --- DynamicObject field helpers (unstructured access via `data`, which is
//     `#[serde(flatten)]` and holds spec/status). ---

fn spec(obj: &DynamicObject) -> &Value {
    obj.data.get("spec").unwrap_or(&Value::Null)
}

fn status(obj: &DynamicObject) -> &Value {
    obj.data.get("status").unwrap_or(&Value::Null)
}

fn i64_of(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn bool_of(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}

fn generation(obj: &DynamicObject) -> i64 {
    obj.metadata.generation.unwrap_or(0)
}

fn observed_generation(obj: &DynamicObject) -> i64 {
    i64_of(status(obj), "observedGeneration").unwrap_or(0)
}

/// Find a condition object by `.type` within `.status.conditions[]`.
fn condition<'a>(status: &'a Value, cond_type: &str) -> Option<&'a Value> {
    status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.iter().find(|c| str_of(c, "type") == Some(cond_type)))
}

fn condition_reason_is(status: &Value, cond_type: &str, reason: &str) -> bool {
    condition(status, cond_type)
        .and_then(|c| str_of(c, "reason"))
        .is_some_and(|r| r == reason)
}

fn condition_status_true(status: &Value, cond_type: &str) -> bool {
    condition(status, cond_type)
        .and_then(|c| str_of(c, "status"))
        .is_some_and(|s| s == "True")
}

fn condition_message(status: &Value, cond_type: &str) -> String {
    condition(status, cond_type)
        .and_then(|c| str_of(c, "message"))
        .unwrap_or("")
        .to_string()
}

/// GVKs with a built-in health check (the dispatch table below, minus the
/// deletion-timestamp precheck). Used so a *missing* live object is only
/// reported `Missing` for kinds we can otherwise assess — unsupported kinds
/// contribute no health signal whether live or absent (Argo CD's
/// `healthCheck == nil` skip).
fn has_health_check(group: &str, kind: &str) -> bool {
    matches!(
        (group, kind),
        ("apps", "Deployment")
            | ("apps", "StatefulSet")
            | ("apps", "ReplicaSet")
            | ("apps", "DaemonSet")
            | ("", "Pod")
            | ("batch", "Job")
            | ("", "Service")
            | ("networking.k8s.io" | "extensions", "Ingress")
            | ("", "PersistentVolumeClaim")
            | ("autoscaling", "HorizontalPodAutoscaler")
            | ("apiregistration.k8s.io", "APIService")
            | ("argoproj.io", "Workflow")
    )
}

/// The health of one live object, dispatching on `(group, kind)`. Returns
/// `None` for GVKs without a built-in check (so they are excluded from the
/// aggregate). A live object with `metadata.deletionTimestamp` set is
/// `Progressing: Pending deletion` regardless of kind.
pub fn get_resource_health(group: &str, kind: &str, obj: &DynamicObject) -> Option<HealthStatus> {
    if obj.metadata.deletion_timestamp.is_some() {
        return Some(HealthStatus::new(
            HealthStatusCode::Progressing,
            "Pending deletion",
        ));
    }
    if !has_health_check(group, kind) {
        return None;
    }
    match (group, kind) {
        ("apps", "Deployment") => Some(deployment_health(obj)),
        ("apps", "StatefulSet") => Some(statefulset_health(obj)),
        ("apps", "ReplicaSet") => Some(replicaset_health(obj)),
        ("apps", "DaemonSet") => Some(daemonset_health(obj)),
        ("", "Pod") => Some(pod_health(obj)),
        ("batch", "Job") => Some(job_health(obj)),
        ("", "Service") => Some(service_health(obj)),
        ("networking.k8s.io" | "extensions", "Ingress") => Some(ingress_health(obj)),
        ("", "PersistentVolumeClaim") => Some(pvc_health(obj)),
        ("autoscaling", "HorizontalPodAutoscaler") => Some(hpa_health(obj)),
        ("apiregistration.k8s.io", "APIService") => Some(apiservice_health(obj)),
        ("argoproj.io", "Workflow") => Some(workflow_health(obj)),
        // Unreachable: has_health_check gates the match above.
        _ => None,
    }
}

/// apps/Deployment (`health_deployment.go`).
fn deployment_health(obj: &DynamicObject) -> HealthStatus {
    let spec = spec(obj);
    let status = status(obj);
    if bool_of(spec, "paused").unwrap_or(false) {
        return HealthStatus::new(HealthStatusCode::Suspended, "Deployment is paused");
    }
    if generation(obj) <= observed_generation(obj) {
        if condition_reason_is(status, "Progressing", "ProgressDeadlineExceeded") {
            return HealthStatus::new(
                HealthStatusCode::Degraded,
                "Deployment exceeded its progress deadline",
            );
        }
        let updated = i64_of(status, "updatedReplicas").unwrap_or(0);
        let replicas = i64_of(status, "replicas").unwrap_or(0);
        let available = i64_of(status, "availableReplicas").unwrap_or(0);
        if let Some(spec_replicas) = i64_of(spec, "replicas") {
            if spec_replicas != updated {
                return progressing(format!(
                    "Waiting for rollout to finish: {updated} out of {spec_replicas} new replicas have been updated..."
                ));
            }
        }
        if replicas > updated {
            return progressing(format!(
                "Waiting for rollout to finish: {replicas} old replicas are pending termination..."
            ));
        }
        if available < updated {
            return progressing(format!(
                "Waiting for rollout to finish: {available} of {updated} updated replicas are available..."
            ));
        }
        HealthStatus::healthy()
    } else {
        progressing(
            "Waiting for rollout to finish: observed deployment generation less than desired generation",
        )
    }
}

/// apps/StatefulSet (`health_statefulset.go`).
fn statefulset_health(obj: &DynamicObject) -> HealthStatus {
    let spec = spec(obj);
    let status = status(obj);
    if observed_generation(obj) == 0 || generation(obj) > observed_generation(obj) {
        return progressing("Waiting for statefulset spec update to be observed...");
    }
    let spec_replicas = i64_of(spec, "replicas");
    let ready = i64_of(status, "readyReplicas").unwrap_or(0);
    if let Some(sr) = spec_replicas {
        if ready < sr {
            return progressing(format!("Waiting for {ready} pods to be ready..."));
        }
    }
    let strategy = str_of(spec, "updateStrategy.type").unwrap_or("");
    if strategy == "RollingUpdate" {
        // Partitioned rollout (partition set): healthy once updatedReplicas
        // reaches (replicas - partition).
        if let (Some(sr), Some(partition)) = (
            spec_replicas,
            spec.get("updateStrategy")
                .and_then(|u| u.get("rollingUpdate"))
                .and_then(|r| i64_of(r, "partition")),
        ) {
            let updated = i64_of(status, "updatedReplicas").unwrap_or(0);
            if updated < sr.saturating_sub(partition) {
                return progressing(format!(
                    "Waiting for partitioned roll out to finish: {updated} out of {} new pods have been updated...",
                    sr.saturating_sub(partition)
                ));
            }
            return HealthStatus::new(
                HealthStatusCode::Healthy,
                format!("partitioned roll out complete: {updated} new pods have been updated..."),
            );
        }
    }
    if strategy == "OnDelete" {
        return HealthStatus::new(
            HealthStatusCode::Healthy,
            format!("statefulset has {ready} ready pods"),
        );
    }
    let current_rev = str_of(status, "currentRevision").unwrap_or("");
    let update_rev = str_of(status, "updateRevision").unwrap_or("");
    if update_rev != current_rev {
        let updated = i64_of(status, "updatedReplicas").unwrap_or(0);
        return progressing(format!(
            "waiting for statefulset rolling update to complete {updated} pods at revision {update_rev}..."
        ));
    }
    HealthStatus::new(
        HealthStatusCode::Healthy,
        format!("statefulset rolling update complete {ready} pods at revision {current_rev}..."),
    )
}

/// apps/ReplicaSet (`health_replicaset.go`).
fn replicaset_health(obj: &DynamicObject) -> HealthStatus {
    let spec = spec(obj);
    let status = status(obj);
    if generation(obj) <= observed_generation(obj) {
        if condition_status_true(status, "ReplicaFailure") {
            return HealthStatus::new(
                HealthStatusCode::Degraded,
                condition_message(status, "ReplicaFailure"),
            );
        }
        if let Some(spec_replicas) = i64_of(spec, "replicas") {
            let available = i64_of(status, "availableReplicas").unwrap_or(0);
            if available < spec_replicas {
                return progressing(format!(
                    "Waiting for rollout to finish: {available} out of {spec_replicas} new replicas are available..."
                ));
            }
        }
        HealthStatus::healthy()
    } else {
        progressing(
            "Waiting for rollout to finish: observed replica set generation less than desired generation",
        )
    }
}

/// apps/DaemonSet (`health_daemonset.go`).
fn daemonset_health(obj: &DynamicObject) -> HealthStatus {
    let spec = spec(obj);
    let status = status(obj);
    if generation(obj) > observed_generation(obj) {
        return progressing(
            "Waiting for rollout to finish: observed daemon set generation less than desired generation",
        );
    }
    let desired = i64_of(status, "desiredNumberScheduled").unwrap_or(0);
    if str_of(spec, "updateStrategy.type") == Some("OnDelete") {
        let updated = i64_of(status, "updatedNumberScheduled").unwrap_or(0);
        return HealthStatus::new(
            HealthStatusCode::Healthy,
            format!("daemon set {updated} out of {desired} new pods have been updated"),
        );
    }
    let updated = i64_of(status, "updatedNumberScheduled").unwrap_or(0);
    if updated < desired {
        return progressing(format!(
            "Waiting for daemon set rollout to finish: {updated} out of {desired} new pods have been updated..."
        ));
    }
    let available = i64_of(status, "numberAvailable").unwrap_or(0);
    if available < desired {
        return progressing(format!(
            "Waiting for daemon set rollout to finish: {available} of {desired} updated pods are available..."
        ));
    }
    HealthStatus::healthy()
}

/// core/Pod (`health_pod.go`).
fn pod_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    let phase = str_of(status, "phase").unwrap_or("");
    let message = str_of(status, "message").unwrap_or("").to_string();
    match phase {
        "Pending" => progressing(message),
        "Succeeded" => HealthStatus::new(HealthStatusCode::Healthy, message),
        "Failed" => HealthStatus::new(HealthStatusCode::Degraded, message),
        "Running" => {
            // A running pod is Healthy once its Ready condition is true; a
            // container stuck in a back-off error is Degraded; otherwise it is
            // still progressing.
            if condition_status_true(status, "Ready") {
                return HealthStatus::new(HealthStatusCode::Healthy, message);
            }
            if let Some(containers) = status.get("containerStatuses").and_then(|c| c.as_array()) {
                for c in containers {
                    if let Some(reason) = c
                        .get("state")
                        .and_then(|s| s.get("waiting"))
                        .and_then(|w| str_of(w, "reason"))
                    {
                        if is_waiting_error(reason) {
                            return HealthStatus::new(
                                HealthStatusCode::Degraded,
                                format!("container stuck in {reason}"),
                            );
                        }
                    }
                }
            }
            progressing(message)
        }
        _ => HealthStatus::new(HealthStatusCode::Unknown, message),
    }
}

/// True for container `waiting.reason` values Argo CD treats as errors
/// (`CrashLoopBackOff`, `ImagePullBackOff`, `ErrImagePull`, `Error`, ...).
fn is_waiting_error(reason: &str) -> bool {
    reason.starts_with("Err") || reason.ends_with("BackOff") || reason == "Error"
}

/// batch/Job (`health_job.go`).
fn job_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    let mut failed = false;
    let mut complete = false;
    let mut suspended = false;
    let mut message = String::new();
    if let Some(conds) = status.get("conditions").and_then(|c| c.as_array()) {
        for c in conds {
            let t = str_of(c, "type").unwrap_or("");
            if str_of(c, "status") != Some("True") {
                continue;
            }
            match t {
                "Complete" => {
                    complete = true;
                    message = str_of(c, "message").unwrap_or("").to_string();
                }
                "Failed" => {
                    failed = true;
                    complete = true;
                    message = str_of(c, "message").unwrap_or("").to_string();
                }
                "Suspended" => {
                    suspended = true;
                    complete = true;
                    message = str_of(c, "message").unwrap_or("").to_string();
                }
                _ => {}
            }
        }
    }
    if !complete {
        return progressing(message);
    }
    if failed {
        return HealthStatus::new(HealthStatusCode::Degraded, message);
    }
    if suspended {
        return HealthStatus::new(HealthStatusCode::Suspended, message);
    }
    HealthStatus::new(HealthStatusCode::Healthy, message)
}

/// core/Service (`health_service.go`).
fn service_health(obj: &DynamicObject) -> HealthStatus {
    let spec = spec(obj);
    let status = status(obj);
    if str_of(spec, "type") != Some("LoadBalancer") {
        return HealthStatus::healthy();
    }
    let has_ingress = status
        .get("loadBalancer")
        .and_then(|lb| lb.get("ingress"))
        .and_then(|i| i.as_array())
        .is_some_and(|a| !a.is_empty());
    if has_ingress {
        HealthStatus::healthy()
    } else {
        progressing("Waiting for loadBalancer ingress to be assigned")
    }
}

/// networking.k8s.io|extensions/Ingress (`health_ingress.go`).
fn ingress_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    let has_ingress = status
        .get("loadBalancer")
        .and_then(|lb| lb.get("ingress"))
        .and_then(|i| i.as_array())
        .is_some_and(|a| !a.is_empty());
    if has_ingress {
        HealthStatus::healthy()
    } else {
        progressing("Waiting for ingress to be assigned")
    }
}

/// core/PersistentVolumeClaim (`health_pvc.go`).
fn pvc_health(obj: &DynamicObject) -> HealthStatus {
    match str_of(status(obj), "phase").unwrap_or("") {
        "Bound" => HealthStatus::healthy(),
        "Pending" => progressing("PVC is pending"),
        "Lost" => HealthStatus::new(HealthStatusCode::Degraded, "PVC is lost"),
        _ => HealthStatus::new(HealthStatusCode::Unknown, ""),
    }
}

/// autoscaling/HorizontalPodAutoscaler (`health_hpa.go`). Condition (type,
/// reason) pairs decide degraded vs healthy; otherwise progressing.
fn hpa_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    let conds = status.get("conditions").and_then(|c| c.as_array());
    let conds = match conds {
        Some(c) if !c.is_empty() => c,
        _ => return progressing("Waiting for HPA to report conditions"),
    };
    for c in conds {
        let t = str_of(c, "type").unwrap_or("");
        let reason = str_of(c, "reason").unwrap_or("");
        let hpa_cond_status = str_of(c, "status").unwrap_or("");
        if is_hpa_degraded(t, reason) {
            return HealthStatus::new(
                HealthStatusCode::Degraded,
                str_of(c, "message").unwrap_or("").to_string(),
            );
        }
        if (t == "AbleToScale" || t == "ScalingLimited") && hpa_cond_status == "True" {
            return HealthStatus::new(
                HealthStatusCode::Healthy,
                str_of(c, "message").unwrap_or("").to_string(),
            );
        }
    }
    progressing("Waiting for HPA to stabilize")
}

/// HPA condition (type, reason) pairs Argo CD classifies as degraded.
fn is_hpa_degraded(cond_type: &str, reason: &str) -> bool {
    matches!(
        (cond_type, reason),
        ("AbleToScale", "FailedGetScale")
            | ("AbleToScale", "FailedUpdateScale")
            | ("ScalingActive", "FailedGetResourceMetric")
            | ("ScalingActive", "FailedGetObjectMetric")
            | ("ScalingActive", "FailedGetPodsMetric")
            | ("ScalingActive", "FailedGetExternalMetric")
            | ("ScalingActive", "FailedComputeMetricsReplicas")
            | ("ScalingActive", "InvalidSelector")
    )
}

/// apiregistration.k8s.io/APIService (`health_apiservice.go`).
fn apiservice_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    match condition(status, "Available") {
        Some(c) => {
            let message = str_of(c, "message").unwrap_or("").to_string();
            match str_of(c, "status") {
                Some("True") => HealthStatus::new(HealthStatusCode::Healthy, message),
                _ => progressing(message),
            }
        }
        None => progressing("Waiting to be processed"),
    }
}

/// argoproj.io/Workflow (`health_argo.go`). Phase → status.
fn workflow_health(obj: &DynamicObject) -> HealthStatus {
    let status = status(obj);
    let phase = str_of(status, "phase").unwrap_or("");
    let message = str_of(status, "message").unwrap_or("").to_string();
    match phase {
        "" | "Pending" | "Running" => progressing(message),
        "Succeeded" => HealthStatus::new(HealthStatusCode::Healthy, message),
        "Failed" | "Error" => HealthStatus::new(HealthStatusCode::Degraded, message),
        _ => HealthStatus::new(HealthStatusCode::Unknown, message),
    }
}

fn progressing(message: impl Into<String>) -> HealthStatus {
    HealthStatus::new(HealthStatusCode::Progressing, message)
}

/// Evaluate the worst health across managed manifests. Each manifest is matched
/// to its live object by name+namespace (`drift::find_live_match`); a missing
/// live object is `Missing`. GVKs without a built-in check contribute nothing
/// (Argo CD's `healthCheck == nil` skip). Returns counts per status plus the
/// worst status spelling. Pure: no API calls.
pub fn assess(manifests: &[RawManifest], live: &[DynamicObject]) -> HealthSummary {
    let live_refs: Vec<&DynamicObject> = live.iter().collect();
    let mut summary = HealthSummary::default();
    let mut worst: Option<HealthStatusCode> = None;
    let mut worst_message = String::new();
    for m in manifests {
        let entry = match crate::drift::find_live_match(m, &live_refs) {
            None => {
                if has_health_check(&m.group, &m.kind) {
                    Some((HealthStatusCode::Missing, String::new()))
                } else {
                    None
                }
            }
            Some(o) => get_resource_health(&m.group, &m.kind, o).map(|h| (h.status, h.message)),
        };
        if let Some((c, msg)) = entry {
            summary.bump(c);
            // Track the worst status and the message from the resource that set it.
            if worst.is_none_or(|w| is_worse(w, c)) {
                worst = Some(c);
                worst_message = msg;
            }
        }
    }
    summary.worst = worst.map(|c| c.as_str().to_string());
    summary.worst_message = worst.map(|_| worst_message);
    summary
}

impl HealthSummary {
    /// Increment the per-status count for `code`.
    pub fn bump(&mut self, code: HealthStatusCode) {
        match code {
            HealthStatusCode::Healthy => self.healthy += 1,
            HealthStatusCode::Progressing => self.progressing += 1,
            HealthStatusCode::Degraded => self.degraded += 1,
            HealthStatusCode::Suspended => self.suspended += 1,
            HealthStatusCode::Missing => self.missing += 1,
            HealthStatusCode::Unknown => self.unknown += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RawManifest;
    use serde_json::{Value, json};

    /// Build a live `DynamicObject` carrying `extra` merged at the top level
    /// (so `spec`/`status` land under `data` via flatten).
    fn live(
        api_version: &str,
        kind: &str,
        name: &str,
        ns: Option<&str>,
        extra: Value,
    ) -> DynamicObject {
        let mut v = json!({
            "apiVersion": api_version,
            "kind": kind,
            "metadata": { "name": name },
        });
        if let Some(n) = ns {
            v["metadata"]["namespace"] = json!(n);
        }
        if let Value::Object(map) = &mut v {
            if let Value::Object(extra) = extra {
                for (k, val) in extra {
                    map.insert(k, val);
                }
            }
        }
        serde_json::from_value(v).expect("live obj parses")
    }

    fn deploy(name: &str, ns: &str, spec: Value, status: Value, generation: i64) -> DynamicObject {
        let mut o = live("apps/v1", "Deployment", name, Some(ns), json!({}));
        o.data = json!({ "spec": spec, "status": status });
        o.metadata.generation = Some(generation);
        o
    }

    // --- is_worse / ordering (Argo CD healthOrder parity) ---

    #[test]
    fn is_worse_matches_health_order() {
        // Healthy is best, Unknown is worst; rank rises as health worsens.
        assert!(!is_worse(
            HealthStatusCode::Degraded,
            HealthStatusCode::Healthy
        ));
        assert!(is_worse(
            HealthStatusCode::Healthy,
            HealthStatusCode::Degraded
        ));
        assert!(is_worse(
            HealthStatusCode::Progressing,
            HealthStatusCode::Missing
        ));
        assert!(is_worse(
            HealthStatusCode::Missing,
            HealthStatusCode::Degraded
        ));
        assert!(is_worse(
            HealthStatusCode::Degraded,
            HealthStatusCode::Unknown
        ));
        assert!(!is_worse(
            HealthStatusCode::Degraded,
            HealthStatusCode::Degraded
        ));
    }

    // --- Deployment ---

    #[test]
    fn deployment_healthy_when_rolled_out() {
        let o = deploy(
            "d",
            "ns",
            json!({"replicas": 2}),
            json!({
                "observedGeneration": 1,
                "updatedReplicas": 2,
                "availableReplicas": 2,
                "replicas": 2,
            }),
            1,
        );
        assert_eq!(
            get_resource_health("apps", "Deployment", &o)
                .unwrap()
                .status,
            HealthStatusCode::Healthy
        );
    }

    #[test]
    fn deployment_progressing_rollout() {
        let o = deploy(
            "d",
            "ns",
            json!({"replicas": 2}),
            json!({
                "observedGeneration": 1,
                "updatedReplicas": 1,
                "availableReplicas": 1,
                "replicas": 2,
            }),
            1,
        );
        assert_eq!(
            get_resource_health("apps", "Deployment", &o)
                .unwrap()
                .status,
            HealthStatusCode::Progressing
        );
    }

    #[test]
    fn deployment_degraded_on_progress_deadline() {
        let o = deploy(
            "d",
            "ns",
            json!({"replicas": 2}),
            json!({
                "observedGeneration": 1,
                "updatedReplicas": 2,
                "availableReplicas": 2,
                "replicas": 2,
                "conditions": [
                    {"type": "Progressing", "reason": "ProgressDeadlineExceeded", "status": "True"}
                ],
            }),
            1,
        );
        assert_eq!(
            get_resource_health("apps", "Deployment", &o)
                .unwrap()
                .status,
            HealthStatusCode::Degraded
        );
    }

    #[test]
    fn deployment_progressing_when_generation_unobserved() {
        let o = deploy("d", "ns", json!({"replicas": 1}), json!({}), 3);
        // observedGeneration absent -> 0 < generation 3.
        assert_eq!(
            get_resource_health("apps", "Deployment", &o)
                .unwrap()
                .status,
            HealthStatusCode::Progressing
        );
    }

    #[test]
    fn deployment_suspended_when_paused() {
        let o = deploy(
            "d",
            "ns",
            json!({"replicas": 1, "paused": true}),
            json!({"observedGeneration": 1, "updatedReplicas": 1, "availableReplicas": 1, "replicas": 1}),
            1,
        );
        assert_eq!(
            get_resource_health("apps", "Deployment", &o)
                .unwrap()
                .status,
            HealthStatusCode::Suspended
        );
    }

    // --- ReplicaSet ---

    #[test]
    fn replicaset_degraded_on_replica_failure() {
        let mut o = live(
            "apps/v1",
            "ReplicaSet",
            "rs",
            Some("ns"),
            json!({
                "spec": {"replicas": 2},
                "status": {
                    "observedGeneration": 1,
                    "availableReplicas": 2,
                    "conditions": [
                        {"type": "ReplicaFailure", "status": "True", "message": "create failed"}
                    ],
                },
            }),
        );
        o.metadata.generation = Some(1);
        assert_eq!(
            get_resource_health("apps", "ReplicaSet", &o)
                .unwrap()
                .status,
            HealthStatusCode::Degraded
        );
    }

    // --- DaemonSet ---

    #[test]
    fn daemonset_healthy_when_scheduled() {
        let mut o = live(
            "apps/v1",
            "DaemonSet",
            "ds",
            Some("ns"),
            json!({
                "spec": {"updateStrategy": {"type": "RollingUpdate"}},
                "status": {
                    "observedGeneration": 1,
                    "desiredNumberScheduled": 3,
                    "updatedNumberScheduled": 3,
                    "numberAvailable": 3,
                },
            }),
        );
        o.metadata.generation = Some(1);
        assert_eq!(
            get_resource_health("apps", "DaemonSet", &o).unwrap().status,
            HealthStatusCode::Healthy
        );
    }

    // --- StatefulSet ---

    #[test]
    fn statefulset_healthy_when_rolled_out() {
        let mut o = live(
            "apps/v1",
            "StatefulSet",
            "sts",
            Some("ns"),
            json!({
                "spec": {"replicas": 2, "updateStrategy": {"type": "RollingUpdate"}},
                "status": {
                    "observedGeneration": 1,
                    "readyReplicas": 2,
                    "updatedReplicas": 2,
                    "currentRevision": "a",
                    "updateRevision": "a",
                },
            }),
        );
        o.metadata.generation = Some(1);
        assert_eq!(
            get_resource_health("apps", "StatefulSet", &o)
                .unwrap()
                .status,
            HealthStatusCode::Healthy
        );
    }

    // --- Pod ---

    #[test]
    fn pod_pending_is_progressing() {
        let o = live(
            "",
            "Pod",
            "p",
            Some("ns"),
            json!({"status": {"phase": "Pending"}}),
        );
        assert_eq!(
            get_resource_health("", "Pod", &o).unwrap().status,
            HealthStatusCode::Progressing
        );
    }

    #[test]
    fn pod_running_ready_is_healthy() {
        let o = live(
            "",
            "Pod",
            "p",
            Some("ns"),
            json!({"status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}}),
        );
        assert_eq!(
            get_resource_health("", "Pod", &o).unwrap().status,
            HealthStatusCode::Healthy
        );
    }

    #[test]
    fn pod_running_crashloop_is_degraded() {
        let o = live(
            "",
            "Pod",
            "p",
            Some("ns"),
            json!({"status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{"state": {"waiting": {"reason": "CrashLoopBackOff"}}}]
            }}),
        );
        assert_eq!(
            get_resource_health("", "Pod", &o).unwrap().status,
            HealthStatusCode::Degraded
        );
    }

    #[test]
    fn pod_failed_is_degraded() {
        let o = live(
            "",
            "Pod",
            "p",
            Some("ns"),
            json!({"status": {"phase": "Failed"}}),
        );
        assert_eq!(
            get_resource_health("", "Pod", &o).unwrap().status,
            HealthStatusCode::Degraded
        );
    }

    // --- Job ---

    #[test]
    fn job_complete_is_healthy() {
        let o = live(
            "batch/v1",
            "Job",
            "j",
            Some("ns"),
            json!({"status": {"conditions": [{"type": "Complete", "status": "True", "message": "ok"}]}}),
        );
        assert_eq!(
            get_resource_health("batch", "Job", &o).unwrap().status,
            HealthStatusCode::Healthy
        );
    }

    #[test]
    fn job_failed_is_degraded() {
        let o = live(
            "batch/v1",
            "Job",
            "j",
            Some("ns"),
            json!({"status": {"conditions": [{"type": "Failed", "status": "True", "message": "boom"}]}}),
        );
        assert_eq!(
            get_resource_health("batch", "Job", &o).unwrap().status,
            HealthStatusCode::Degraded
        );
    }

    #[test]
    fn job_running_is_progressing() {
        let o = live("batch/v1", "Job", "j", Some("ns"), json!({"status": {}}));
        assert_eq!(
            get_resource_health("batch", "Job", &o).unwrap().status,
            HealthStatusCode::Progressing
        );
    }

    // --- Service / Ingress / PVC ---

    #[test]
    fn service_clusterip_is_healthy() {
        let o = live(
            "",
            "Service",
            "s",
            Some("ns"),
            json!({"spec": {"type": "ClusterIP"}, "status": {}}),
        );
        assert_eq!(
            get_resource_health("", "Service", &o).unwrap().status,
            HealthStatusCode::Healthy
        );
    }

    #[test]
    fn service_loadbalancer_progressing_without_ingress() {
        let o = live(
            "",
            "Service",
            "s",
            Some("ns"),
            json!({"spec": {"type": "LoadBalancer"}, "status": {}}),
        );
        assert_eq!(
            get_resource_health("", "Service", &o).unwrap().status,
            HealthStatusCode::Progressing
        );
    }

    #[test]
    fn ingress_progressing_without_ingress() {
        let o = live(
            "networking.k8s.io/v1",
            "Ingress",
            "i",
            Some("ns"),
            json!({"status": {}}),
        );
        assert_eq!(
            get_resource_health("networking.k8s.io", "Ingress", &o)
                .unwrap()
                .status,
            HealthStatusCode::Progressing
        );
    }

    #[test]
    fn pvc_bound_is_healthy() {
        let o = live(
            "",
            "PersistentVolumeClaim",
            "pvc",
            Some("ns"),
            json!({"status": {"phase": "Bound"}}),
        );
        assert_eq!(
            get_resource_health("", "PersistentVolumeClaim", &o)
                .unwrap()
                .status,
            HealthStatusCode::Healthy
        );
    }

    // --- deletion timestamp (applies to any kind) ---

    #[test]
    fn deleting_object_is_progressing() {
        let o: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "p", "namespace": "ns", "deletionTimestamp": "2024-01-01T00:00:00Z"},
            "status": {"phase": "Running"}
        }))
        .unwrap();
        assert_eq!(
            get_resource_health("", "Pod", &o).unwrap().status,
            HealthStatusCode::Progressing
        );
    }

    // --- unsupported GVK ---

    #[test]
    fn unsupported_gvk_returns_none() {
        let o = live(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "v"}}),
        );
        assert!(get_resource_health("", "ConfigMap", &o).is_none());
    }

    // --- assess: aggregation ---

    fn manifest(group: &str, version: &str, kind: &str, name: &str, ns: &str) -> RawManifest {
        let api_version = if group.is_empty() {
            version.to_string()
        } else {
            format!("{group}/{version}")
        };
        let data = serde_yaml::to_string(&json!({
            "apiVersion": api_version,
            "kind": kind,
            "metadata": {"name": name, "namespace": ns},
        }))
        .unwrap()
        .into_bytes();
        RawManifest {
            group: group.to_string(),
            version: version.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: Some(ns.to_string()),
            data,
            annotations: Default::default(),
        }
    }

    #[test]
    fn assess_worst_is_degraded_and_counts_statuses() {
        let manifests = vec![
            manifest("apps", "v1", "Deployment", "healthy-d", "ns"),
            manifest("apps", "v1", "Deployment", "degraded-d", "ns"),
            manifest("apps", "v1", "Deployment", "missing-d", "ns"),
            manifest("", "v1", "ConfigMap", "cm", "ns"),
        ];
        let live = vec![
            deploy(
                "healthy-d",
                "ns",
                json!({"replicas": 1}),
                json!({"observedGeneration": 1, "updatedReplicas": 1, "availableReplicas": 1, "replicas": 1}),
                1,
            ),
            deploy(
                "degraded-d",
                "ns",
                json!({"replicas": 1}),
                json!({
                    "observedGeneration": 1,
                    "updatedReplicas": 1,
                    "availableReplicas": 1,
                    "replicas": 1,
                    "conditions": [{"type": "Progressing", "reason": "ProgressDeadlineExceeded", "status": "True"}],
                }),
                1,
            ),
        ];
        let summary = assess(&manifests, &live);
        assert_eq!(summary.worst.as_deref(), Some("Degraded"));
        assert_eq!(summary.healthy, 1);
        assert_eq!(summary.degraded, 1);
        assert_eq!(summary.missing, 1);
        assert_eq!(
            summary.worst_message.as_deref(),
            Some("Deployment exceeded its progress deadline")
        );
        // ConfigMap has no built-in check -> not counted at all.
        assert_eq!(summary.healthy + summary.degraded + summary.missing, 3);
    }

    #[test]
    fn assess_all_healthy() {
        let manifests = vec![manifest("apps", "v1", "Deployment", "d", "ns")];
        let live = vec![deploy(
            "d",
            "ns",
            json!({"replicas": 1}),
            json!({"observedGeneration": 1, "updatedReplicas": 1, "availableReplicas": 1, "replicas": 1}),
            1,
        )];
        let summary = assess(&manifests, &live);
        assert_eq!(summary.worst.as_deref(), Some("Healthy"));
        assert_eq!(summary.healthy, 1);
    }

    #[test]
    fn assess_no_checkable_yields_no_worst() {
        // Only ConfigMaps: no built-in health check, so worst stays None.
        let manifests = vec![manifest("", "v1", "ConfigMap", "c", "ns")];
        let live = vec![live(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "v"}}),
        )];
        let summary = assess(&manifests, &live);
        assert!(summary.worst.is_none());
        assert_eq!(summary.healthy, 0);
    }

    #[test]
    fn health_status_code_as_str_matches_argo_spelling() {
        // The persisted `worst` and the metric label use these exact spellings
        // (Argo CD parity), so they must stay stable.
        assert_eq!(HealthStatusCode::Healthy.as_str(), "Healthy");
        assert_eq!(HealthStatusCode::Progressing.as_str(), "Progressing");
        assert_eq!(HealthStatusCode::Degraded.as_str(), "Degraded");
        assert_eq!(HealthStatusCode::Suspended.as_str(), "Suspended");
        assert_eq!(HealthStatusCode::Missing.as_str(), "Missing");
        assert_eq!(HealthStatusCode::Unknown.as_str(), "Unknown");
    }
}
