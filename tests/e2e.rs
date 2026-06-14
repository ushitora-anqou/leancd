//! End-to-end tests for leancd.
//!
//! Each scenario drives leancd and Forgejo as in-cluster Pods on an ephemeral
//! `kind` cluster and asserts the behaviour described in `doc/design.md`.
//! Every test is `#[ignore]` because it needs Docker + kind; run them with
//! `make e2e`
//! (== `cargo test --test e2e -- --ignored --test-threads=1 --nocapture`).
//!
//! By default `cargo test` / `nextest` skip `#[ignore]` tests, so this file
//! stays out of `nix flake check` (which runs in a sandbox without Docker) —
//! the same status as `make bench` (design §8.4).

mod common;

use common::manifests;
use std::time::Duration;

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
    let count: u64 = st["data"]["sync_count"]
        .as_str()
        .expect("sync_count present")
        .parse()
        .expect("sync_count numeric");
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
    let data = st["data"].as_object().expect("state has data");
    let sha = data["last_sha"].as_str().expect("last_sha present");
    assert!(!sha.is_empty(), "last_sha must be non-empty");
    let count: u64 = data["sync_count"]
        .as_str()
        .expect("sync_count present")
        .parse()
        .expect("sync_count numeric");
    assert!(count >= 1);
    let managed: u64 = data["managed_count"]
        .as_str()
        .expect("managed_count present")
        .parse()
        .expect("managed_count numeric");
    assert!(managed >= 1);
    let applied = data["applied"].as_str().expect("applied present");
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

/// Scenario 8: `/metrics` exposes the always-present metric series and a sane
/// RSS reading. `leancd_drift_detected` is a labelled gauge that only emits a
/// series while drift is present, so it is exercised indirectly by the drift
/// scenario (drift self-heal implies detection worked) rather than here.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires docker + kind; run with: make e2e"]
async fn metrics() {
    common::Fixture::get();
    let text = common::metrics::scrape();
    for name in [
        "leancd_sync_total",
        "leancd_sync_errors_total",
        "leancd_sync_last_success_timestamp_seconds",
        "leancd_managed_resources",
        "leancd_rss_bytes",
    ] {
        assert!(text.contains(name), "metric {name} missing:\n{text}");
    }
    let rss = common::metrics::metric_value(&text, "leancd_rss_bytes").expect("rss readable");
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

/// Scenario 12: another field manager owns `data.k`; a normal sync cannot take
/// it over, but `sync --force` claims it.
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

    // Normal sync: leancd cannot take over data.k (conflict), field stays "other".
    assert!(common::leancd::sync(&args).success);
    let cm = common::kubectl::get_json("default", "configmap", "fc-cm");
    assert_eq!(
        cm["data"]["k"],
        serde_json::json!("other"),
        "normal sync must not take over a field owned by another manager"
    );

    // --force sync: claims the field.
    let mut force_args = args.clone();
    force_args.push("--force".to_string());
    assert!(common::leancd::sync(&force_args).success);
    let cm = common::kubectl::get_json("default", "configmap", "fc-cm");
    assert_eq!(
        cm["data"]["k"],
        serde_json::json!("v"),
        "sync --force must take over the conflicting field"
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
    let err = st["data"]["last_error"].as_str();
    assert!(err.is_some(), "last_error should be recorded");
    assert!(!err.unwrap().is_empty(), "last_error should be non-empty");
}
