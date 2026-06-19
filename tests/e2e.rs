//! End-to-end tests for leancd.
//!
//! Each scenario drives leancd and Forgejo as in-cluster Pods on an ephemeral
//! `kind` cluster and asserts leancd's intended behaviour.
//! Every test is `#[ignore]` because it needs Docker + kind; run them with
//! `make e2e`
//! (== `cargo test --test e2e -- --ignored --test-threads=1 --nocapture`).
//!
//! By default `cargo test` / `nextest` skip `#[ignore]` tests, so this file
//! stays out of `nix flake check` (which runs in a sandbox without Docker) —
//! the same status as `make bench`.

mod common;

use common::manifests;
use std::time::Duration;

/// Parse the unified `state` JSON value stored in a state ConfigMap's `data`.
fn state_json(cm: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(cm["data"]["state"].as_str().expect("`state` key present"))
        .expect("`state` value is valid JSON")
}

/// Scenario 1: the first sync applies a mix of namespaced and cluster-scoped
/// resources, each carrying the managed-by label, and writes sync state.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn initial_apply() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("initial-apply");
    fj.create_repo(&env.repo, false);
    let _ = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "cm.yaml".into(),
                manifests::configmap("ia-cm", "default", &[("k", "v")]),
            ),
            ("namespace.yaml".into(), manifests::namespace("ia-ns")),
            ("clusterrole.yaml".into(), manifests::clusterrole("ia-cr")),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync failed: {}", res.stderr);

    assert!(common::kubectl::exists("default", "configmap", "ia-cm"));
    assert!(common::kubectl::exists("", "namespace", "ia-ns"));
    assert!(common::kubectl::exists("", "clusterrole", "ia-cr"));

    let cm = common::kubectl::get_json("default", "configmap", "ia-cm");
    assert_eq!(
        cm["metadata"]["labels"][common::env::MANAGED_LABEL_KEY],
        serde_json::json!(env.label_value)
    );
    let cr = common::kubectl::get_json("", "clusterrole", "ia-cr");
    assert_eq!(
        cr["metadata"]["labels"][common::env::MANAGED_LABEL_KEY],
        serde_json::json!(env.label_value)
    );

    let st = common::kubectl::get_json(&env.namespace, "configmap", &env.state_cm);
    let s = state_json(&st);
    let count: u64 = s["sync_count"].as_u64().expect("sync_count numeric");
    assert!(count >= 1, "sync_count should be >= 1, got {count}");
}

/// Scenario 2: a Git HEAD move triggers a full apply (new resource appears),
/// then a second sync with no Git change takes the drift-check path and does
/// NOT re-apply (resourceVersion stable). A subsequent push re-engages full
/// apply.
///
/// Steady-state is asserted via the stable resourceVersion rather than the
/// `leancd_sync_total`/`leancd_drift_detected` metrics: each `sync` runs as a
/// short-lived `kubectl exec` process, so its `/metrics` does not persist for a
/// scrape, and the long-lived controller Deployment (the only scrapeable
/// endpoint) watches a different repo. resourceVersion stability is the
/// precise signal that no re-apply happened.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn git_change_detection_and_steady_state() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("change-steady");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("cs-cm", "default", &[("v", "1")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));

    let r1 = common::leancd::sync(&args);
    assert!(r1.success, "1st sync failed: {}", r1.stderr);
    assert!(common::kubectl::exists("default", "configmap", "cs-cm"));
    let rv1 = common::kubectl::get_json("default", "configmap", "cs-cm")["metadata"]
        ["resourceVersion"]
        .as_str()
        .unwrap()
        .to_string();

    let r2 = common::leancd::sync(&args);
    assert!(r2.success, "2nd sync failed: {}", r2.stderr);
    let rv2 = common::kubectl::get_json("default", "configmap", "cs-cm")["metadata"]
        ["resourceVersion"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rv1, rv2,
        "steady-state sync must not re-apply (resourceVersion changed)"
    );

    common::git::push_more(
        &clone,
        fj,
        &env.repo,
        &[(
            "cm2.yaml".into(),
            manifests::configmap("cs-cm2", "default", &[("v", "2")]),
        )],
    );
    let r3 = common::leancd::sync(&args);
    assert!(r3.success, "3rd sync failed: {}", r3.stderr);
    assert!(
        common::wait::wait_for(
            || common::kubectl::exists("default", "configmap", "cs-cm2"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "cs-cm2 should appear after the changed-HEAD sync"
    );
}

/// Scenario 3: an out-of-band deletion drifts the cluster; a short-poll
/// controller detects it (via the drift-check path) and re-applies.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn drift_self_heal() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("drift");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("dr-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists("default", "configmap", "dr-cm"));

    // Out-of-band deletion: drift the cluster away from Git.
    common::kubectl::delete("default", "configmap", "dr-cm");
    assert!(!common::kubectl::exists("default", "configmap", "dr-cm"));

    // A polling controller must self-heal.
    let _ctrl = common::leancd::controller("leancd-ctrl-drift", args.clone());
    let healed = common::wait::wait_for(
        || common::kubectl::exists("default", "configmap", "dr-cm"),
        Duration::from_secs(30),
        Duration::from_millis(500),
    );
    assert!(healed, "controller did not self-heal the deleted ConfigMap");
}

/// Scenario 4: a resource removed from Git is pruned; an unmanaged resource
/// (no leancd label, never in the applied set) survives.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn prune() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("prune");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "cm1.yaml".into(),
                manifests::configmap("pr-cm1", "default", &[("k", "1")]),
            ),
            (
                "cm2.yaml".into(),
                manifests::configmap("pr-cm2", "default", &[("k", "2")]),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists("default", "configmap", "pr-cm1"));
    assert!(common::kubectl::exists("default", "configmap", "pr-cm2"));

    // An unmanaged ConfigMap (no leancd label) — must survive pruning.
    common::kubectl::apply_stdin(&manifests::configmap(
        "pr-unmanaged",
        "default",
        &[("k", "x")],
    ));

    // Remove pr-cm2 from Git and re-sync.
    common::git::remove_and_push(&clone, fj, &env.repo, &["cm2.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::kubectl::exists("default", "configmap", "pr-cm1"),
        "pr-cm1 must remain"
    );
    assert!(
        !common::kubectl::exists("default", "configmap", "pr-cm2"),
        "pr-cm2 must be pruned"
    );
    assert!(
        common::kubectl::exists("default", "configmap", "pr-unmanaged"),
        "unmanaged resource must survive pruning"
    );
}

/// Scenario 5: the state ConfigMap records last SHA, counts, and the applied
/// key set after a sync.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn state_configmap() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("state");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("st-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);

    let st = common::kubectl::get_json(&env.namespace, "configmap", &env.state_cm);
    let s = state_json(&st);
    let sha = s["last_sha"].as_str().expect("last_sha present");
    assert!(!sha.is_empty(), "last_sha must be non-empty");
    let count: u64 = s["sync_count"].as_u64().expect("sync_count numeric");
    assert!(count >= 1);
    let managed: u64 = s["managed_count"].as_u64().expect("managed_count numeric");
    assert!(managed >= 1);
    // `applied` is a JSON array of ResourceKey objects in the unified state.
    let applied = serde_json::to_string(&s["applied"]).expect("applied present");
    assert!(
        applied.contains("ConfigMap"),
        "applied must list ConfigMap: {applied}"
    );
    assert!(
        applied.contains("st-cm"),
        "applied must list st-cm: {applied}"
    );
}

/// Scenario 6: `leancd sync` runs a single reconciliation and exits 0 with the
/// resource applied.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn cli_sync_once() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("cli-sync");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("csync-cm", "default", &[("k", "v")]),
        )],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync should exit 0: {}", res.stderr);
    assert!(common::kubectl::exists("default", "configmap", "csync-cm"));
}

/// Scenario 7: `leancd status` prints the persisted state in a human-readable
/// format (matching `main.rs::run_status`).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn cli_status() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("cli-status");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("cstat-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    let res = common::leancd::status(&args);
    assert!(res.success, "status failed: {}", res.stderr);
    let out = &res.stdout;
    assert!(
        out.contains("leancd status"),
        "missing status header: {out}"
    );
    assert!(out.contains("last sha:"), "missing last sha line: {out}");
    assert!(
        out.contains("sync count: 1"),
        "missing sync count line: {out}"
    );
    assert!(out.contains("managed:"), "missing managed line: {out}");
}

/// Scenario 8: leancd's metrics reach the OTel collector and re-export on its
/// Prometheus endpoint with a sane RSS reading. `leancd_drift_detected` only
/// emits a series while drift is present, so it is exercised indirectly by the
/// drift scenario (drift self-heal implies detection worked) rather than here.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn metrics() {
    common::Fixture::get();
    let text = common::metrics::scrape();
    // leancd pushes several distinct metric families (at least 5:
    // sync_total/sync_errors_total/last_success_timestamp_seconds/
    // managed_resources/rss_bytes; drift_detected is added only while drift is
    // present, so it is exercised by the drift scenario instead). Count unique
    // families rather than exact names so the OTel→Prometheus `_total`/unit
    // renaming does not make this assertion brittle.
    let families: std::collections::HashSet<String> = text
        .lines()
        .filter(|l| l.starts_with("leancd_") && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|t| t.split('{').next().map(String::from))
        .collect();
    assert!(
        families.len() >= 5,
        "expected at least 5 leancd metric families, got {}: {:?}\n{text}",
        families.len(),
        families
    );
    // RSS is pushed as leancd_rss_bytes; take the most recent sample value.
    let rss = text
        .lines()
        .filter(|l| l.starts_with("leancd_rss_bytes") && !l.starts_with('#'))
        .filter_map(|l| l.rsplit(' ').next())
        .filter_map(|v| v.parse::<f64>().ok().map(|f| f as i64))
        .next_back()
        .expect("leancd_rss_bytes sample present");
    assert!(rss > 0, "rss should be positive");
    assert!(
        rss < 100 * 1024 * 1024,
        "rss should stay under the 100MiB budget: {rss} bytes"
    );
}

/// Scenario 9: cluster-scoped (Namespace, ClusterRole) and namespaced
/// (ConfigMap) resources are applied together, exercising the Scope branch in
/// `kube_util::api_for`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn cluster_and_namespaced_scope() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("scope");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            ("ns.yaml".into(), manifests::namespace("sc-ns")),
            ("cr.yaml".into(), manifests::clusterrole("sc-cr")),
            (
                "cm.yaml".into(),
                manifests::configmap("sc-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    assert!(common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo))).success);
    assert!(common::kubectl::exists("", "namespace", "sc-ns"));
    assert!(common::kubectl::exists("", "clusterrole", "sc-cr"));
    assert!(common::kubectl::exists("default", "configmap", "sc-cm"));
}

/// Scenario 10: leancd fetches a private Forgejo repo over HTTPS using basic
/// auth credentials from the Secret. A successful apply proves auth worked.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn https_basic_auth() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("https");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("https-cm", "default", &[("k", "v")]),
        )],
    );
    assert!(common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo))).success);
    assert!(common::kubectl::exists("default", "configmap", "https-cm"));
}

/// Scenario 11: leancd fetches over SSH using a registered public key and an
/// injected `GIT_SSH_KEY`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn ssh_key_auth() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("ssh");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("ssh-cm", "default", &[("k", "v")]),
        )],
    );

    let (pub_key, priv_key) = common::ssh::generate_keypair();
    fj.add_ssh_key(&pub_key);

    let res = common::leancd::sync_ssh(
        "leancd-ssh-sync",
        &priv_key,
        &env.sync_args(&fj.ssh_url(&env.repo)),
    );
    assert!(res.success, "ssh sync failed:\n{}", res.stdout);
    assert!(common::kubectl::exists("default", "configmap", "ssh-cm"));
}

/// Scenario 15: a `kind: List` and a `---`-separated document in one file are
/// both expanded and applied.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn multi_doc_list() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("multidoc");
    fj.create_repo(&env.repo, false);
    let yaml = "\
apiVersion: v1
kind: List
items:
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: md-list-a
      namespace: default
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: md-list-b
      namespace: default
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: md-doc-c
  namespace: default
";
    common::git::push_files(fj, &env.repo, &[("all.yaml".into(), yaml.to_string())]);
    assert!(common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo))).success);
    assert!(common::kubectl::exists("default", "configmap", "md-list-a"));
    assert!(common::kubectl::exists("default", "configmap", "md-list-b"));
    assert!(common::kubectl::exists("default", "configmap", "md-doc-c"));
}

/// Scenario 12: another field manager owns `data.k`; a normal sync claims it
/// (SSA always applies with force, so conflicting fields are reclaimed).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn force_conflict() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("force");

    // Another field manager creates and owns data.k on the ConfigMap.
    let other = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: fc-cm\n  namespace: default\ndata:\n  k: \"other\"\n";
    common::kubectl::apply_ssa(other, "other-manager");

    // Git declares the same ConfigMap with data.k=v.
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("fc-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));

    // A single normal sync claims the conflicting field (SSA is always forced).
    assert!(common::leancd::sync(&args).success);
    let cm = common::kubectl::get_json("default", "configmap", "fc-cm");
    assert_eq!(
        cm["data"]["k"],
        serde_json::json!("v"),
        "sync must take over a field owned by another manager (SSA is always forced)"
    );
}

/// Scenario 13: a CRD and a custom resource are applied in separate syncs (the
/// CRD must be established before the CR can be discovered and applied).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn crd_apply() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("crd");
    fj.create_repo(&env.repo, false);

    let crd = "apiVersion: apiextensions.k8s.io/v1\n\
kind: CustomResourceDefinition\n\
metadata:\n  name: leancdtests.e2e.leancd\n\
spec:\n  group: e2e.leancd\n  scope: Namespaced\n  names:\n    kind: LeancdTest\n    listKind: LeancdTestList\n    plural: leancdtests\n    singular: leancdtest\n  versions:\n    - name: v1\n      served: true\n      storage: true\n      schema:\n        openAPIV3Schema:\n          type: object\n          properties:\n            spec:\n              type: object\n              properties:\n                value:\n                  type: string\n";
    let clone = common::git::push_files(fj, &env.repo, &[("crd.yaml".into(), crd.to_string())]);
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(
        common::wait::wait_for(
            || common::kubectl::exists("", "crd", "leancdtests.e2e.leancd"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "CRD should be applied"
    );

    let cr = "apiVersion: e2e.leancd/v1\nkind: LeancdTest\nmetadata:\n  name: crd-test\n  namespace: default\nspec:\n  value: hello\n";
    common::git::push_more(&clone, fj, &env.repo, &[("cr.yaml".into(), cr.to_string())]);
    assert!(common::leancd::sync(&args).success);
    assert!(
        common::wait::wait_for(
            || common::kubectl::exists("default", "leancdtest", "crd-test"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "custom resource should be applied after the CRD"
    );
}

/// Scenario 14: a polling controller reflects a Git push without any manual
/// `sync` (the controller loop reconciles on its poll interval).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn controller_polling() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("ctrl");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("ctrl-cm", "default", &[("k", "1")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    // Seed state so the controller takes the drift-check path until the HEAD moves.
    assert!(common::leancd::sync(&args).success);

    // Start a short-poll controller.
    let _ctrl = common::leancd::controller("leancd-ctrl-poll", args.clone());

    // Push a new resource; the controller must pick it up within a few polls.
    common::git::push_more(
        &clone,
        fj,
        &env.repo,
        &[(
            "cm2.yaml".into(),
            manifests::configmap("ctrl-cm2", "default", &[("k", "2")]),
        )],
    );
    assert!(
        common::wait::wait_for(
            || common::kubectl::exists("default", "configmap", "ctrl-cm2"),
            Duration::from_secs(30),
            Duration::from_millis(500),
        ),
        "polling controller did not reflect the pushed change"
    );
}

/// Scenario 16: an unparseable YAML document is skipped, valid manifests in
/// the same push are applied, and sync still exits 0.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn error_recovery_skip_bad_manifest() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("errskip");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "good.yaml".into(),
                manifests::configmap("er-good", "default", &[("k", "v")]),
            ),
            // A document missing apiVersion parses fine but is skipped by
            // `value_to_manifest` (not a recognised manifest); the valid
            // ConfigMap is still applied and sync exits 0.
            (
                "bad.yaml".into(),
                "kind: ConfigMap\nmetadata:\n  name: er-bad\n".to_string(),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(
        res.success,
        "sync should exit 0 despite the bad doc: {}",
        res.stderr
    );
    assert!(common::kubectl::exists("default", "configmap", "er-good"));
    assert!(
        !common::kubectl::exists("default", "configmap", "er-bad"),
        "the bad manifest must not be applied"
    );
}

/// Scenario 17: an unreachable repository URL makes sync fail (non-zero), and
/// the error is recorded in the state ConfigMap's last_error.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn git_unreachable_records_error() {
    common::Fixture::get();
    let env = common::env::TestEnv::new("unreach");
    let args =
        env.sync_args("http://forgejo.forgejo.svc.cluster.local:3000/leancd/no-such-repo.git");
    let res = common::leancd::sync(&args);
    assert!(
        !res.success,
        "sync of an unreachable repo should exit non-zero"
    );

    let st = common::kubectl::get_json(&env.namespace, "configmap", &env.state_cm);
    let s = state_json(&st);
    let err = s["last_error"].as_str();
    assert!(err.is_some(), "last_error should be recorded");
    assert!(!err.unwrap().is_empty(), "last_error should be non-empty");
}

// --- Helm Hook scenarios (helm.sh/hook) ---
//
// Hooks run in phases around the main apply (PreSync → main → PostSync, or
// PreDelete → prune → PostDelete on a full teardown). A hook is a resource
// carrying a `helm.sh/hook` annotation; leancd classifies it, orders it by
// `helm.sh/hook-weight`, applies it, awaits Job/Pod completion, and deletes it
// per `helm.sh/hook-delete-policy`. Hooks never enter the applied set, so they
// are not pruned. The hook container is the `leancd:latest` image (loaded into
// the kind node) running `/bin/sh -c`, so a hook's effect is observable only
// via its own status and the presence/absence of resources.

/// Scenario 18 (hook A): a PreSync Job hook runs before the main apply. Both
/// the hook Job and the main ConfigMap remain (default `before-hook-creation`
/// only deletes a prior instance on the *next* run; with no delete policy the
/// hook is kept after it succeeds). The Job's `.status.succeeded` proves it was
/// awaited to completion, not merely applied.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn presync_job_runs_before_main() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-presync");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "hh-a-job",
                    "default",
                    "pre-install",
                    None,
                    None,
                    "echo pre-sync; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("hh-a-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync failed: {}", res.stderr);
    assert!(common::kubectl::exists("default", "job", "hh-a-job"));
    assert!(common::kubectl::exists("default", "configmap", "hh-a-cm"));

    let job = common::kubectl::get_json("default", "job", "hh-a-job");
    assert_eq!(
        job["status"]["succeeded"].as_i64(),
        Some(1),
        "pre-sync hook Job should complete: {:?}",
        job["status"]
    );
}

/// Scenario 19 (hook B): a failing PreSync hook aborts the pass — the main
/// resource is never applied, sync exits non-zero, and the failure is recorded
/// in the state ConfigMap's `last_error`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn presync_failure_aborts_main() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-presync-fail");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook("hh-b-job", "default", "pre-install", None, None, "exit 1"),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("hh-b-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(!res.success, "sync should fail when a pre-sync hook fails");
    assert!(
        !common::kubectl::exists("default", "configmap", "hh-b-cm"),
        "main resource must not be applied when pre-sync hook fails"
    );

    let st = common::kubectl::get_json(&env.namespace, "configmap", &env.state_cm);
    let s = state_json(&st);
    let err = s["last_error"]
        .as_str()
        .expect("last_error should be recorded");
    assert!(
        err.contains("pre-sync hook failed"),
        "last_error should mention the pre-sync hook failure: {err}"
    );
}

/// Scenario 20 (hook C): a PreSync hook with `hook-delete-policy: hook-succeeded`
/// is deleted by leancd once it completes successfully, while the main resource
/// is applied and kept.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn hook_succeeded_deletes_hook() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-del");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "hh-c-job",
                    "default",
                    "pre-install",
                    None,
                    Some("hook-succeeded"),
                    "echo pre-sync; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("hh-c-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync failed: {}", res.stderr);
    let gone = common::wait::wait_for(
        || !common::kubectl::exists("default", "job", "hh-c-job"),
        Duration::from_secs(15),
        Duration::from_millis(500),
    );
    assert!(
        gone,
        "hook-succeeded Job should be deleted after completion"
    );
    assert!(common::kubectl::exists("default", "configmap", "hh-c-cm"));
}

/// Scenario 21 (hook D): a hook is excluded from the applied set (only the
/// ConfigMap is listed), and after the hook is removed from Git it is NOT
/// pruned — it was never in the applied set, so it is neither a primary-diff
/// target nor a safety-net candidate.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn hook_not_in_applied_set() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-applied");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "hh-d-job",
                    "default",
                    "pre-install",
                    None,
                    None,
                    "echo pre-sync; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("hh-d-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);

    let st = common::kubectl::get_json(&env.namespace, "configmap", &env.state_cm);
    let applied =
        serde_json::to_string(&state_json(&st)["applied"]).expect("applied set present in state");
    assert!(
        applied.contains("hh-d-cm"),
        "applied set must list the ConfigMap: {applied}"
    );
    assert!(
        !applied.contains("hh-d-job"),
        "applied set must NOT list the hook Job: {applied}"
    );

    // Remove the hook from Git; it must survive prune.
    common::git::remove_and_push(&clone, fj, &env.repo, &["pre.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::kubectl::exists("default", "job", "hh-d-job"),
        "hook Job must survive prune (never in the applied set)"
    );
    assert!(
        common::kubectl::exists("default", "configmap", "hh-d-cm"),
        "ConfigMap still declared in Git must remain"
    );
}

/// Scenario 22 (hook E): a main resource carrying
/// `helm.sh/resource-policy: keep` is not pruned when it leaves Git (teardown
/// runs prune, but `keep` exempts the resource from deletion).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn resource_policy_keep_survives_prune() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-keep");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap_keep("hh-e-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists("default", "configmap", "hh-e-cm"));

    common::git::remove_and_push(&clone, fj, &env.repo, &["cm.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::kubectl::exists("default", "configmap", "hh-e-cm"),
        "resource-policy=keep ConfigMap must survive prune"
    );
}

/// Scenario 23 (hook F): a PostSync Job hook runs after the main apply; both
/// the main ConfigMap and the hook Job remain, and the Job completed.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn postsync_hook_after_main() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-postsync");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "cm.yaml".into(),
                manifests::configmap("hh-f-cm", "default", &[("k", "v")]),
            ),
            (
                "post.yaml".into(),
                manifests::job_hook(
                    "hh-f-job",
                    "default",
                    "post-install",
                    None,
                    None,
                    "echo post-sync; exit 0",
                ),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync failed: {}", res.stderr);
    assert!(common::kubectl::exists("default", "configmap", "hh-f-cm"));
    assert!(common::kubectl::exists("default", "job", "hh-f-job"));

    let job = common::kubectl::get_json("default", "job", "hh-f-job");
    assert_eq!(
        job["status"]["succeeded"].as_i64(),
        Some(1),
        "post-sync hook Job should complete: {:?}",
        job["status"]
    );
}

/// Scenario 24 (hook G): a PreSync Pod hook (not a Job) is awaited to its
/// terminal `phase`; leancd waits for `phase=Succeeded` before applying main.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn pod_hook_completion() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-pod");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::pod_hook(
                    "hh-g-pod",
                    "default",
                    "pre-install",
                    None,
                    None,
                    "echo pre-sync; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("hh-g-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(res.success, "sync failed: {}", res.stderr);
    assert!(common::kubectl::exists("default", "configmap", "hh-g-cm"));

    let pod = common::kubectl::get_json("default", "pod", "hh-g-pod");
    assert_eq!(
        pod["status"]["phase"].as_str(),
        Some("Succeeded"),
        "pre-sync Pod hook should reach phase=Succeeded: {:?}",
        pod["status"]
    );
}

/// Scenario 25 (hook H): removing the last main resource from Git (keeping a
/// pre-delete hook) triggers a full teardown. The pre-delete hook runs (it does
/// NOT run on a normal sync), then the main ConfigMap is pruned.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn teardown_with_pre_delete() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-teardown");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "cm.yaml".into(),
                manifests::configmap("hh-h-cm", "default", &[("k", "v")]),
            ),
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "hh-h-pre",
                    "default",
                    "pre-delete",
                    None,
                    None,
                    "echo pre-delete; exit 0",
                ),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists("default", "configmap", "hh-h-cm"));
    assert!(
        !common::kubectl::exists("default", "job", "hh-h-pre"),
        "pre-delete hook must not run on a normal (non-teardown) sync"
    );

    // Remove the main resource, keep the pre-delete hook → full teardown.
    common::git::remove_and_push(&clone, fj, &env.repo, &["cm.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "configmap", "hh-h-cm"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "main ConfigMap must be pruned during teardown"
    );
    assert!(
        common::kubectl::exists("default", "job", "hh-h-pre"),
        "pre-delete hook Job should be applied during teardown"
    );
    let job = common::kubectl::get_json("default", "job", "hh-h-pre");
    assert_eq!(
        job["status"]["succeeded"].as_i64(),
        Some(1),
        "pre-delete hook should complete during teardown: {:?}",
        job["status"]
    );
}

/// Scenario 26 (hook I): within a phase, hooks run in ascending `hook-weight`.
/// The lower-weight hook (-5) runs first and fails, so the higher-weight hook
/// (+5) never runs. Both hooks use the default delete policy (kept after the
/// run), so the higher-weight hook's absence proves it was skipped.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn weight_ordering_aborts_early() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("hh-weight");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "low.yaml".into(),
                manifests::job_hook(
                    "hh-i-low",
                    "default",
                    "pre-install",
                    Some(-5),
                    None,
                    "exit 1",
                ),
            ),
            (
                "high.yaml".into(),
                manifests::job_hook(
                    "hh-i-high",
                    "default",
                    "pre-install",
                    Some(5),
                    None,
                    "echo high; exit 0",
                ),
            ),
        ],
    );
    let res = common::leancd::sync(&env.sync_args(&fj.https_url(&env.repo)));
    assert!(
        !res.success,
        "sync should fail when the first-weight hook fails"
    );
    assert!(
        common::kubectl::exists("default", "job", "hh-i-low"),
        "lower-weight hook should have been applied"
    );
    assert!(
        !common::kubectl::exists("default", "job", "hh-i-high"),
        "higher-weight hook must not run after the lower-weight hook fails"
    );

    let low = common::kubectl::get_json("default", "job", "hh-i-low");
    assert_eq!(
        low["status"]["failed"].as_i64(),
        Some(1),
        "lower-weight hook should have failed: {:?}",
        low["status"]
    );
}

/// Scenario: a controller pointed at an unreachable repo fails every pass and
/// backs off — the delay between attempts grows above the poll interval — while
/// staying alive. The exact backoff curve (and the [0.75x, 1.0x) jitter) is
/// unit-tested in `reconcile::tests`; here we assert the integration contract:
/// the controller emits "backing off" across consecutive failures and keeps
/// running rather than crashing.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn backoff_on_repeated_failures() {
    common::Fixture::get();
    let env = common::env::TestEnv::new("backoff");
    // An unreachable repo URL: every reconcile fails (git fetch error).
    let args =
        env.sync_args("http://forgejo.forgejo.svc.cluster.local:3000/leancd/no-such-repo.git");
    let _ctrl = common::leancd::controller("leancd-backoff", args);

    // Wait until the controller logs at least two backoff lines (>=2 failed
    // passes). Do not assert exact seconds: the delay is jittered and depends
    // on clone latency.
    let backed_off = common::wait::wait_for(
        || job_logs("leancd-backoff").matches("backing off").count() >= 2,
        Duration::from_secs(60),
        Duration::from_millis(1000),
    );
    assert!(
        backed_off,
        "controller did not back off on repeated failures; logs:\n{}",
        job_logs("leancd-backoff")
    );

    // The controller pod must still be Running (backs off, does not crash).
    let phase = common::kubectl::get_json("leancd", "pod", &pod_for_job("leancd-backoff"))
        ["status"]["phase"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(
        phase, "Running",
        "controller should stay alive while backing off"
    );
}

/// Scenario: a controller receiving SIGTERM (pod deletion) finishes its
/// in-flight pass and exits 0 within the grace period — cooperative shutdown,
/// not a mid-pass abort. `terminationGracePeriodSeconds: 30` on the controller
/// Job wraps the `--shutdown-timeout-secs` default of 28s.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn graceful_shutdown_finishes_pass() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("graceful");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("graceful-cm", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    let _ctrl = common::leancd::controller("leancd-graceful", args);

    // Wait for the pod, then send SIGTERM by deleting it.
    let started = common::wait::wait_for(
        || common::kubectl::pod_name_by_selector("leancd", "job-name=leancd-graceful").is_some(),
        Duration::from_secs(30),
        Duration::from_millis(500),
    );
    assert!(started, "controller pod did not start");
    let pod = common::kubectl::pod_name_by_selector("leancd", "job-name=leancd-graceful")
        .expect("controller pod vanished");

    // Wait for the first reconcile to finish: `shutdown_signal()` installs its
    // SIGTERM handler in parallel with the loop right after spawning it, so
    // once the first pass completes the handler is ready — sending SIGTERM
    // earlier races handler installation and is dropped (PID 1 ignores
    // unhandled SIGTERM).
    let reconciled = common::wait::wait_for(
        || job_logs("leancd-graceful").contains("reconciliation complete"),
        Duration::from_secs(60),
        Duration::from_millis(500),
    );
    assert!(
        reconciled,
        "controller did not finish its first reconcile; logs:\n{}",
        job_logs("leancd-graceful")
    );

    // Send SIGTERM to leancd (PID 1) from inside the pod. Unlike `kubectl
    // delete pod`, this leaves the pod object in place, so its Terminated
    // status (exit code) stays readable (restartPolicy: Never keeps it).
    let kill = std::process::Command::new("kubectl")
        .args([
            "exec",
            "-n",
            "leancd",
            &pod,
            "--",
            "/bin/sh",
            "-c",
            "kill -TERM 1",
        ])
        .output();
    assert!(
        kill.as_ref().map(|o| o.status.success()).unwrap_or(false),
        "kubectl exec kill -TERM 1 failed: {:?}",
        kill.map(|o| String::from_utf8_lossy(&o.stderr).to_string())
    );

    // The pod reaches Terminated with leancd's exit code.
    let terminated = common::wait::wait_for(
        || {
            !common::kubectl::get_json("leancd", "pod", &pod)["status"]["containerStatuses"][0]
                ["state"]["terminated"]
                .is_null()
        },
        Duration::from_secs(45),
        Duration::from_millis(500),
    );
    assert!(terminated, "pod did not terminate after SIGTERM");

    let st = common::kubectl::get_json("leancd", "pod", &pod)["status"]["containerStatuses"][0]
        ["state"]["terminated"]
        .clone();
    assert_eq!(
        st["exitCode"].as_i64(),
        Some(0),
        "graceful shutdown should exit 0: {st}"
    );
    assert_eq!(
        st["reason"].as_str(),
        Some("Completed"),
        "terminated reason should be Completed: {st}"
    );
}

/// Scenario: `leancd health` exit code tracks the sync lifecycle — never (1)
/// before the first sync, fresh (0) after a successful sync, failing (3) after
/// a sync that recorded an error.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn health_lifecycle() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("health");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("health-cm", "default", &[("k", "v")]),
        )],
    );
    let valid = env.sync_args(&fj.https_url(&env.repo));

    // (1) No state yet -> never synced -> exit 1.
    assert_eq!(
        common::leancd::health(&valid).exit_code,
        1,
        "health should be 'never' before the first sync"
    );

    // (2) A successful sync -> fresh -> exit 0.
    assert!(common::leancd::sync(&valid).success, "sync should succeed");
    assert_eq!(
        common::leancd::health(&valid).exit_code,
        0,
        "health should be 'fresh' after a successful sync"
    );

    // (3) A failed sync records last_error -> failing -> exit 3.
    let unreachable =
        env.sync_args("http://forgejo.forgejo.svc.cluster.local:3000/leancd/no-such-repo.git");
    assert!(
        !common::leancd::sync(&unreachable).success,
        "sync of an unreachable repo should fail"
    );
    assert_eq!(
        common::leancd::health(&valid).exit_code,
        3,
        "health should be 'failing' after a sync recorded an error"
    );
}

/// Scenario: the harness leancd Deployment pod runs under Pod Security
/// Standards "restricted" and is Running. Static assertions on the pod spec
/// (every restricted field is already set in tests/leancd.yaml).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn pss_restricted_deploy_pod() {
    common::Fixture::get();
    let found = common::wait::wait_for(
        || {
            common::kubectl::pod_name_by_selector("leancd", "app.kubernetes.io/name=leancd")
                .is_some()
        },
        Duration::from_secs(30),
        Duration::from_millis(500),
    );
    assert!(found, "deploy/leancd pod not found");
    let pod = common::kubectl::pod_name_by_selector("leancd", "app.kubernetes.io/name=leancd")
        .expect("deploy/leancd pod vanished");

    let p = common::kubectl::get_json("leancd", "pod", &pod);
    assert_eq!(
        p["status"]["phase"].as_str(),
        Some("Running"),
        "deploy/leancd pod should be Running"
    );

    // Pod-level restricted security context.
    assert_eq!(
        p["spec"]["securityContext"]["runAsNonRoot"].as_bool(),
        Some(true),
        "runAsNonRoot must be true"
    );
    assert_eq!(
        p["spec"]["securityContext"]["seccompProfile"]["type"].as_str(),
        Some("RuntimeDefault"),
        "seccompProfile must be RuntimeDefault"
    );

    // Container-level restricted security context.
    let sc = &p["spec"]["containers"][0]["securityContext"];
    assert_eq!(
        sc["runAsUser"].as_i64(),
        Some(65532),
        "runAsUser must be 65532 (nonroot)"
    );
    assert_eq!(
        sc["readOnlyRootFilesystem"].as_bool(),
        Some(true),
        "readOnlyRootFilesystem must be true"
    );
    let drops: Vec<&str> = sc["capabilities"]["drop"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        drops.contains(&"ALL"),
        "capabilities.drop must contain ALL, got {drops:?}"
    );
}

// --- Foreground cascade deletion scenarios ---
//
// leancd deletes every resource with `DeleteParams::foreground()`. Foreground
// cascade stamps a `foregroundDeletion` finalizer on the owner and removes its
// dependents first; background deletion does neither. To prove foreground
// deterministically (without racing the kind GC), each scenario parks a *stall*
// finalizer on a dependent so the owner lingers behind `foregroundDeletion`,
// which we assert on. Releasing the stall finalizer lets the cascade finish so
// nothing litters the shared cluster.

/// Scenario (fgdelete A): pruning a resource removed from Git uses foreground
/// cascade. An unmanaged dependent ConfigMap (never in the applied set, so
/// leancd never prunes it directly) carries an explicit ownerReference to the
/// pruned owner and a stall finalizer; that holds the owner behind a
/// `foregroundDeletion` finalizer — present only under foreground cascade.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn fgdelete_prune() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("fg-prune");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[(
            "owner.yaml".into(),
            manifests::configmap("fg-pr-owner", "default", &[("k", "v")]),
        )],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists(
        "default",
        "configmap",
        "fg-pr-owner"
    ));

    // Unmanaged dependent (no leancd label -> never pruned directly). Make it a
    // dependent of the owner and stall it.
    common::kubectl::apply_stdin(&manifests::configmap(
        "fg-pr-child",
        "default",
        &[("k", "child")],
    ));
    common::fgdelete::link_owner(
        "default",
        "configmap",
        "fg-pr-child",
        "ConfigMap",
        "fg-pr-owner",
        "v1",
    );
    common::fgdelete::add_stall_finalizer("default", "configmap", "fg-pr-child");

    // Remove the owner from Git; leancd prunes it in the foreground.
    common::git::remove_and_push(&clone, fj, &env.repo, &["owner.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::fgdelete::wait_for_foreground(
            "default",
            "configmap",
            "fg-pr-owner",
            Duration::from_secs(30),
        ),
        "owner must carry the foregroundDeletion finalizer (foreground cascade); \
         background deletion removes it immediately with no finalizer"
    );

    // Release the dependent so the cascade completes and nothing lingers.
    common::fgdelete::remove_stall_finalizer("default", "configmap", "fg-pr-child");
    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "configmap", "fg-pr-owner"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "owner must disappear once the dependent's stall finalizer is released"
    );
}

/// Scenario (fgdelete B): a full teardown (main set emptied, a pre-delete hook
/// kept) prunes every main resource in the foreground. Same dependent/stall
/// technique as fgdelete A, exercised through the teardown path
/// (pre-delete -> prune-all -> post-delete).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn fgdelete_teardown() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("fg-teardown");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "cm.yaml".into(),
                manifests::configmap("fg-td-owner", "default", &[("k", "v")]),
            ),
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "fg-td-pre",
                    "default",
                    "pre-delete",
                    None,
                    None,
                    "echo pre-delete; exit 0",
                ),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists(
        "default",
        "configmap",
        "fg-td-owner"
    ));
    // pre-delete hook does NOT run on a normal (non-teardown) sync.
    assert!(!common::kubectl::exists("default", "job", "fg-td-pre"));

    common::kubectl::apply_stdin(&manifests::configmap(
        "fg-td-child",
        "default",
        &[("k", "child")],
    ));
    common::fgdelete::link_owner(
        "default",
        "configmap",
        "fg-td-child",
        "ConfigMap",
        "fg-td-owner",
        "v1",
    );
    common::fgdelete::add_stall_finalizer("default", "configmap", "fg-td-child");

    // Remove the main resource, keep the pre-delete hook -> full teardown.
    common::git::remove_and_push(&clone, fj, &env.repo, &["cm.yaml".to_string()]);
    assert!(common::leancd::sync(&args).success);

    assert!(
        common::fgdelete::wait_for_foreground(
            "default",
            "configmap",
            "fg-td-owner",
            Duration::from_secs(30),
        ),
        "owner must carry the foregroundDeletion finalizer when pruned during teardown"
    );

    common::fgdelete::remove_stall_finalizer("default", "configmap", "fg-td-child");
    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "configmap", "fg-td-owner"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "owner must disappear once the dependent's stall finalizer is released"
    );
}

/// Scenario (fgdelete C): the `before-hook-creation` delete policy (the default)
/// removes a prior hook instance in the foreground on the *next* sync. After the
/// first sync leaves the hook Job and its Pod in place, we stall the Pod and
/// re-sync: leancd deletes the old Job in the foreground (before applying the
/// new one), so it lingers behind `foregroundDeletion` while the stalled Pod
/// blocks it. We do not assert sync success: leancd does not wait for the
/// foreground delete to finish before re-applying, so the re-apply may collide
/// with the still-terminating Job (a before-hook-creation caveat unrelated to
/// what we observe) — the delete has already fired in the foreground by then.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn fgdelete_hook_before_creation() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("fg-bhc");
    fj.create_repo(&env.repo, false);
    let clone = common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "fg-bc-job",
                    "default",
                    "pre-install",
                    None,
                    None,
                    "echo pre-sync; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("fg-bc-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));
    assert!(common::leancd::sync(&args).success);
    assert!(common::kubectl::exists("default", "job", "fg-bc-job"));
    let pod = common::kubectl::pod_name_by_selector("default", "job-name=fg-bc-job")
        .expect("hook Job should have a Pod after the first sync");
    common::fgdelete::add_stall_finalizer("default", "pod", &pod);

    // Move HEAD so the next sync takes the full-apply path: a no-op second
    // sync would drift-check and never re-run the PreSync hook, so
    // before-hook-creation would not fire. Tweaking the main ConfigMap is
    // enough to move HEAD.
    common::git::push_more(
        &clone,
        fj,
        &env.repo,
        &[(
            "cm.yaml".into(),
            manifests::configmap("fg-bc-cm", "default", &[("k", "v2")]),
        )],
    );

    // Second sync: before-hook-creation fires a foreground delete of the prior
    // Job. Sync may report failure from the re-apply collision, but the delete
    // has already happened in the foreground — which is what we observe.
    let _ = common::leancd::sync(&args);

    assert!(
        common::fgdelete::wait_for_foreground(
            "default",
            "job",
            "fg-bc-job",
            Duration::from_secs(30),
        ),
        "hook Job must carry the foregroundDeletion finalizer (foreground cascade)"
    );

    common::fgdelete::remove_stall_finalizer("default", "pod", &pod);
    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "job", "fg-bc-job"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "hook Job must disappear once the Pod's stall finalizer is released"
    );
}

/// Scenario (fgdelete D): the `hook-succeeded` delete policy removes a
/// completed hook in the foreground, within the same sync that ran it. Because
/// the delete fires the instant the hook completes, we run the sync in the
/// background and park a stall finalizer on the hook Pod *while* it runs — the
/// Job script sleeps long enough to make that window deterministic. Once the
/// hook succeeds, leancd deletes the Job in the foreground; the stalled Pod
/// blocks it, so the Job lingers behind `foregroundDeletion`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn fgdelete_hook_succeeded() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("fg-hs");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "fg-hs-job",
                    "default",
                    "pre-install",
                    None,
                    Some("hook-succeeded"),
                    "sleep 60; exit 0",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("fg-hs-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));

    // Run sync in the background: the hook sleeps 60s, giving us time to stall
    // its Pod before leancd deletes the completed Job.
    let handle = common::leancd::sync_handle(args);
    let found = common::wait::wait_for(
        || common::kubectl::pod_name_by_selector("default", "job-name=fg-hs-job").is_some(),
        Duration::from_secs(60),
        Duration::from_millis(500),
    );
    assert!(found, "hook Job Pod should appear while the hook runs");
    let pod = common::kubectl::pod_name_by_selector("default", "job-name=fg-hs-job")
        .expect("hook Job Pod vanished before we could stall it");
    common::fgdelete::add_stall_finalizer("default", "pod", &pod);

    // Let the sync finish: hook succeeds -> hook-succeeded deletes the Job in
    // the foreground. The stalled Pod keeps the Job behind foregroundDeletion.
    let res = handle.join();
    assert!(res.success, "sync failed: {}", res.stderr);

    assert!(
        common::fgdelete::wait_for_foreground(
            "default",
            "job",
            "fg-hs-job",
            Duration::from_secs(30),
        ),
        "hook Job must carry the foregroundDeletion finalizer (foreground cascade)"
    );

    common::fgdelete::remove_stall_finalizer("default", "pod", &pod);
    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "job", "fg-hs-job"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "hook Job must disappear once the Pod's stall finalizer is released"
    );
}

/// Scenario (fgdelete E): the `hook-failed` delete policy removes a failed hook
/// in the foreground, within the same sync — the mirror of fgdelete D with a
/// non-zero hook exit. A PreSync failure aborts the pass (sync reports failure),
/// but the hook-failed delete has already fired in the foreground, which is what
/// we observe; we do not assert sync success.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn fgdelete_hook_failed() {
    let f = common::Fixture::get();
    let fj = f.forgejo();
    let env = common::env::TestEnv::new("fg-hf");
    fj.create_repo(&env.repo, false);
    common::git::push_files(
        fj,
        &env.repo,
        &[
            (
                "pre.yaml".into(),
                manifests::job_hook(
                    "fg-hf-job",
                    "default",
                    "pre-install",
                    None,
                    Some("hook-failed"),
                    "sleep 60; exit 1",
                ),
            ),
            (
                "cm.yaml".into(),
                manifests::configmap("fg-hf-cm", "default", &[("k", "v")]),
            ),
        ],
    );
    let args = env.sync_args(&fj.https_url(&env.repo));

    let handle = common::leancd::sync_handle(args);
    let found = common::wait::wait_for(
        || common::kubectl::pod_name_by_selector("default", "job-name=fg-hf-job").is_some(),
        Duration::from_secs(60),
        Duration::from_millis(500),
    );
    assert!(found, "hook Job Pod should appear while the hook runs");
    let pod = common::kubectl::pod_name_by_selector("default", "job-name=fg-hf-job")
        .expect("hook Job Pod vanished before we could stall it");
    common::fgdelete::add_stall_finalizer("default", "pod", &pod);

    // Let the sync finish: hook fails -> hook-failed deletes the Job in the
    // foreground. The stalled Pod keeps the Job behind foregroundDeletion.
    let res = handle.join();
    eprintln!(
        "fgdelete_hook_failed sync exit_code={} stderr={}",
        res.exit_code, res.stderr
    );

    assert!(
        common::fgdelete::wait_for_foreground(
            "default",
            "job",
            "fg-hf-job",
            Duration::from_secs(30),
        ),
        "hook Job must carry the foregroundDeletion finalizer (foreground cascade)"
    );

    common::fgdelete::remove_stall_finalizer("default", "pod", &pod);
    assert!(
        common::wait::wait_for(
            || !common::kubectl::exists("default", "job", "fg-hf-job"),
            Duration::from_secs(15),
            Duration::from_millis(500),
        ),
        "hook Job must disappear once the Pod's stall finalizer is released"
    );
}

/// Read the logs of a leancd-namespace controller Job.
fn job_logs(name: &str) -> String {
    std::process::Command::new("kubectl")
        .args(["logs", "-n", "leancd", &format!("job/{name}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// The pod name backing a leancd-namespace Job (panics if none).
fn pod_for_job(job: &str) -> String {
    common::kubectl::pod_name_by_selector("leancd", &format!("job-name={job}"))
        .unwrap_or_else(|| panic!("no pod found for job {job}"))
}
