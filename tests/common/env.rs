//! Per-scenario isolation: unique repo, state ConfigMap, work dir, managed
//! label value. Each scenario constructs a `TestEnv` so its resources never
//! collide with another's (cluster-scoped resources included, via the label).

/// The managed-by label key Lean CD injects (matches cli.rs default).
pub const MANAGED_LABEL_KEY: &str = "app.kubernetes.io/managed-by";

pub struct TestEnv {
    pub scenario: String,
    pub repo: String,
    pub branch: String,
    pub state_cm: String,
    pub namespace: String,
    pub label_value: String,
}

impl TestEnv {
    pub fn new(scenario: &str) -> TestEnv {
        let scenario = scenario.to_string();
        TestEnv {
            repo: format!("e2e-{scenario}"),
            branch: "main".into(),
            state_cm: format!("leancd-state-{scenario}"),
            namespace: "leancd".into(),
            label_value: format!("leancd-e2e-{scenario}"),
            scenario,
        }
    }

    /// Common Lean CD flags pinning this scenario's repo/branch/state/work dir
    /// and managed-label value. The repo URL is supplied by the caller (it
    /// differs for HTTPS vs SSH transports).
    pub fn sync_args(&self, repo_url: &str) -> Vec<String> {
        vec![
            "--repo-url".into(),
            repo_url.into(),
            "--branch".into(),
            self.branch.clone(),
            "--state-configmap".into(),
            self.state_cm.clone(),
            "--managed-label-value".into(),
            self.label_value.clone(),
            "--work-dir".into(),
            format!("/tmp/leancd-{}", self.scenario),
        ]
    }
}
