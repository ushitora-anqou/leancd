# Migrating from Argo CD to leancd

This is a hands-on, command-level guide for moving a cluster that Argo CD
synces over to be managed by **leancd** — gradually, in place, with no big-bang
cutover and no downtime. Every step below was validated end-to-end on a local
`kind` cluster running Argo CD and leancd side by side.

> For leancd's full feature reference see [`./user-manual.md`](./user-manual.md);
> for a first-time deploy walkthrough see [`./tutorial.md`](./tutorial.md); for
> internals see [`./architecture.md`](./architecture.md); for the project
> overview see [`../README.md`](../README.md).

## The one rule you must not break

> **Never let leancd and another tool manage the same resource.** Hand off
> ownership of each resource — atomically, one resource (or one namespace) at a
> time — so that at every instant exactly one tool owns it.

This rule exists because leancd's safety model is **destructive when
violated**:

- Every resource leancd applies gets the label
  `app.kubernetes.io/managed-by=leancd` injected at apply time
  (`manifest.rs`, `inject_managed_label`).
- leancd's pruner deletes, as a safety-net, any **live** resource carrying that
  label that is **not** in leancd's current Git set (`prune.rs`).

So if leancd is ever pointed at a resource another tool owns, it stamps the
label on it — and the moment that resource later leaves leancd's Git path, the
pruner deletes it, **including resources the other tool still depends on**.
During validation this exact mechanism deleted an Argo-managed namespace's
contents when leancd's path was narrowed; see [§5](#5-optional-observe-why-co-management-is-forbidden)
and the warning in [§6](#6-phase-1--hand-off-one-namespace).

A secondary reason: leancd applies with server-side apply `.force()`
(`kube_util.rs`, `PatchParams::apply(field_manager).force()`), and Argo CD syncs
with `--force-conflicts`. Two tools co-managing the same resource turns every
field into a tug-of-war. (Validation showed that if both tools apply the
*identical* manifest the result is actually stable — no `resourceVersion` churn —
but co-management is still forbidden because of the pruner behaviour above, not
because of thrashing.)

## 0. What you will do

```
 start                              mid-migration                       end
 ┌─────────────────┐                ┌─────────────────┐                ┌─────────────────┐
 │ Argo CD owns    │  hand off      │ app-a : Argo CD  │  hand off      │ leancd owns     │
 │  everything     │ ──ns by ns──▶  │ app-b : leancd  │ ──rest──▶      │  everything     │
 │ leancd owns     │                │ (each resource  │                │ Argo CD removed │
 │  nothing        │                │  one owner)     │                │                 │
 └─────────────────┘                └─────────────────┘                └─────────────────┘
```

The handoff mechanism, repeated per namespace:

1. Stop Argo auto-sync (so Argo will not prune a resource mid-handoff).
2. Move that namespace's manifests from Argo's Git path into leancd's Git path.
3. Let leancd take ownership (first run = full apply, `.force()` reclaims the
   fields).
4. Repeat until Argo's Git path is empty.
5. Disarm Argo's cascade finalizer, delete the Argo Application (orphan —
   resources survive), then uninstall Argo CD.

## 1. Prerequisites

| Tool | Why | Check |
|---|---|---|
| `docker` | build the leancd image, run kind | `docker --version` |
| `kind` | the local cluster | `kind version` |
| `kubectl` | talk to the cluster | `kubectl version --client` |
| `argocd` | drive Argo CD (login, repo add, sync) | `argocd version --client` |
| `git` | prepare the repository | `git --version` |
| `curl` | create the Forgejo repo | `curl --version` |

`argocd` is not in the leancd flake devShell; install it separately (for
example `nix profile install nixpkgs#argocd` — adjust to your package manager).

**Repo format:** plain YAML only. leancd does not render Helm/Kustomize/Jsonnet.
If Argo CD renders charts for you, pre-render them (`helm template`, `kustomize
build`) and commit the resulting YAML before starting.

## 2. leancd behaviours that shape the migration

Keep these in mind throughout:

- **Empty paths are refused.** leancd will not start with a path that matches no
  directory: `config error: no directories matched path pattern(s) [...];
  refusing to sync as that would prune every managed resource`. This is a safety
  feature. To run leancd while it owns nothing yet, point it at a real but
  *empty* directory (a `.gitkeep` placeholder).
- **First run = full apply.** With no prior state, leancd applies everything in
  its path and reclaims fields via `.force()` (`reconcile.rs`, `should_full_apply`).
- **`managed-by=leancd` is permanent until removed.** Once leancd stamps it, the
  pruner treats the resource as leancd's. Removing leancd's ownership cleanly is
  done by having the resource recreated (see [§11](#11-rollback--abort)).
- **Argo CD hooks are invisible to leancd.** leancd reads `helm.sh/hook` only;
  `argocd.argoproj.io/hook` and `argocd.argoproj.io/sync-wave` are ignored, so an
  Argo hook is applied as an ordinary resource with no phase ordering, no
  completion wait, and no auto-delete. See [§8](#8-convert-argo-cd-hooks).
- **State is one ConfigMap** (`leancd-state`), deliberately *unlabelled* so the
  pruner never deletes it. Argo CD must never manage the `leancd` namespace.

## 3. Stand up the validation cluster (kind + Forgejo + Argo CD + leancd)

This mirrors [`./tutorial.md`](./tutorial.md) §4c (in-cluster Forgejo) and adds
Argo CD into the same cluster.

```sh
kind create cluster --name migration
```

### 3a. leancd image

```sh
docker build -t leancd:latest .
kind load docker-image leancd:latest --name migration
```

### 3b. In-cluster Forgejo (leancd's and Argo CD's shared Git server)

```sh
kubectl apply -f tests/forgejo.yaml
kubectl wait -n forgejo --for=condition=Available deploy/forgejo --timeout=240s
```

Create the admin user, then (second terminal) port-forward and create the repo:

```sh
kubectl -n forgejo exec deploy/forgejo -- \
  su -c "forgejo admin user create --admin \
    --username leancd --password leancd-e2e-pass \
    --email leancd@test.local --must-change-password=false \
    -c /data/gitea/conf/app.ini -w /var/lib/gitea" git

kubectl -n forgejo port-forward svc/forgejo 3000:3000   # second terminal

curl -sS -X POST -u leancd:leancd-e2e-pass -H "Content-Type: application/json" \
  -d '{"name":"manifests","private":true,"auto_init":true,"default_branch":"main"}' \
  http://127.0.0.1:3000/api/v1/user/repos
```

Clone it and seed an empty placeholder so leancd has a valid (non-empty) path to
own nothing from yet — recall leancd refuses an empty path:

```sh
git clone http://127.0.0.1:3000/leancd/manifests.git
cd manifests
mkdir -p leancd-managed && touch leancd-managed/.gitkeep
git add -A && git -c user.email=d@d -c user.name=demo commit -qm "leancd placeholder"
git push    # user leancd / pass leancd-e2e-pass
```

The repository now has two top-level areas we will move resources between:

- `argocd-managed/` — Argo CD's source path (we fill this in [§4](#4-reproduce-the-argo-cd-managed-starting-state)).
- `leancd-managed/` — leancd's source path (starts empty).

### 3c. Argo CD

```sh
kubectl create namespace argocd
kubectl apply -n argocd --server-side --force-conflicts \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml
kubectl wait -n argocd --for=condition=Available deploy/argocd-server --timeout=600s
```

Retrieve the admin password, port-forward the server, log in, and register the
repo so Argo CD can read the private Forgejo repository:

```sh
ARGO_PW=$(kubectl -n argocd get secret argocd-initial-admin-secret \
  -o jsonpath='{.data.password}' | base64 -d)
kubectl -n argocd port-forward svc/argocd-server 8080:443 &   # second terminal
argocd login 127.0.0.1:8080 --username admin --password "$ARGO_PW" --insecure
argocd repo add http://forgejo.forgejo.svc.cluster.local:3000/leancd/manifests.git \
  --username leancd --password leancd-e2e-pass
```

### 3d. leancd

Install the chart pointed at the Forgejo repo and the `leancd-managed` path:

```sh
kubectl create namespace leancd
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-literal=GIT_USERNAME=leancd --from-literal=GIT_PASSWORD=leancd-e2e-pass
helm install leancd charts/leancd \
  --namespace leancd --create-namespace \
  --set config.repoUrl=http://forgejo.forgejo.svc.cluster.local:3000/leancd/manifests.git \
  --set config.path=leancd-managed \
  --set image.repository=leancd --set image.tag=latest
kubectl -n leancd wait --for=condition=Available deploy/leancd --timeout=240s
kubectl -n leancd logs deploy/leancd --tail=2
# expect: reconciliation complete ... managed=0 ...   (owns nothing yet)
```

The cluster now runs Argo CD and leancd side by side, each pointed at a
disjoint path of the same repo.

## 4. Reproduce the Argo CD-managed starting state

Commit plain YAML under `argocd-managed/` spanning **two namespaces**, a
**CRD + custom resource**, and an **Argo CD PreSync/PostSync hook**. (These are
the constructs whose migration is non-obvious.)

```sh
cd manifests
mkdir -p argocd-managed/namespaces/app-a argocd-managed/namespaces/app-b \
         argocd-managed/cluster argocd-managed/hooks
```

`argocd-managed/namespaces/app-a/configmap.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: app-a
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-a
  namespace: app-a
data:
  greeting: hello-from-argo-a
```

`argocd-managed/namespaces/app-a/deployment.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: dep-a
  namespace: app-a
spec:
  replicas: 1
  selector:
    matchLabels: { app: dep-a }
  template:
    metadata:
      labels: { app: dep-a }
    spec:
      terminationGracePeriodSeconds: 1
      containers:
        - name: main
          image: leancd:latest          # already on the kind node
          imagePullPolicy: IfNotPresent
          command: ["/bin/sh", "-c"]
          args: ["sleep 3600"]
```

`argocd-managed/namespaces/app-b/configmap.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: app-b
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-b
  namespace: app-b
data:
  greeting: hello-from-argo-b
```

`argocd-managed/cluster/crd.yaml` (the same CRD leancd's own e2e suite uses):

```yaml
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: leancdtests.e2e.leancd
spec:
  group: e2e.leancd
  scope: Namespaced
  names:
    kind: LeancdTest
    listKind: LeancdTestList
    plural: leancdtests
    singular: leancdtest
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              properties:
                value:
                  type: string
```

`argocd-managed/cluster/customresource.yaml`:

```yaml
apiVersion: e2e.leancd/v1
kind: LeancdTest
metadata:
  name: crd-test
  namespace: app-a
spec:
  value: hello
```

`argocd-managed/hooks/presync-job.yaml` and `postsync-job.yaml` — Argo CD hooks
we will convert in [§8](#8-convert-argo-cd-hooks):

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: presync-hook          # postsync-job.yaml uses name: postsync-hook
  namespace: app-a
  annotations:
    argocd.argoproj.io/hook: PreSync   # PostSync in the other file
    argocd.argoproj.io/sync-wave: "0"
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: hook
          image: leancd:latest
          imagePullPolicy: IfNotPresent
          command: ["/bin/sh", "-c"]
          args: ["echo presync-hook-ran; exit 0"]   # postsync-hook-ran in the other
```

Commit and push, then declare the Argo CD `Application` (directory of plain
YAML, automated sync, server-side apply, create namespaces):

```sh
git add -A && git commit -qm "argo-managed: 2 ns, CRD+CR, PreSync/PostSync hooks" && git push
```

`/tmp/argocd-app.yaml`:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: demo
  namespace: argocd
  finalizers:
    - resources-finalizer.argocd.argoproj.io   # cascade delete — handled in §9
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: app-a
  project: default
  source:
    repoURL: http://forgejo.forgejo.svc.cluster.local:3000/leancd/manifests.git
    targetRevision: main
    path: argocd-managed
    directory:
      recurse: true
  syncPolicy:
    automated: {}
    syncOptions:
      - CreateNamespace=true
      - ServerSideApply=true
```

```sh
kubectl apply -f /tmp/argocd-app.yaml
argocd app sync demo --server-side
argocd app get demo | grep -E "Sync Status|Health Status"      # Synced / Healthy
```

Confirm Argo CD owns everything and leancd is still idle:

```sh
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.managedFields[*].manager}'
# argocd-controller            (no "leancd")
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.labels}'   # {} — no managed-by
kubectl -n leancd logs deploy/leancd --tail=1 | grep reconciliation
# managed=0  (leancd owns nothing, as intended)
```

## 5. (Optional) Observe why co-management is forbidden

This section is for understanding. **Undo it immediately after, exactly as
shown**, or [§6](#6-phase-1--hand-off-one-namespace) will destroy Argo's
resources (see the warning there).

Point leancd at the *same* path Argo manages and let it reconcile once:

```sh
kubectl -n leancd set env deployment/leancd LEANCD_PATH=argocd-managed
kubectl -n leancd logs deploy/leancd --tail=1 | grep reconciliation
# managed=9 drift=9 ... then managed=9 drift=0   (leancd now "owns" them too)
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.managedFields[*].manager}'
# argocd-controller leancd        (two managers — co-management)
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.labels}'
# {"app.kubernetes.io/managed-by":"leancd"}     (leancd stamped the label)
```

Because both apply the identical manifest, `resourceVersion` does **not** churn —
the conflict is latent, not active. The danger is the label: those resources are
now in leancd's prune set. Undo by **fully resetting leancd** so the label does
not outlive the demo — and verify Argo recreates the resources clean:

```sh
kubectl -n leancd scale deploy leancd --replicas=0
kubectl -n leancd delete cm leancd-state
kubectl -n leancd set env deployment/leancd LEANCD_PATH=leancd-managed
kubectl -n leancd scale deploy leancd --replicas=1
# Re-sync Argo so it recreates the labelled resources WITHOUT managed-by:
argocd app sync demo --server-side
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.labels}'    # {} again
```

> **The lesson, concretely:** once `managed-by=leancd` is on a resource, the
> only clean way to take it off leancd's books is to have the resource
> recreated (Argo re-sync does this). If instead you just narrow leancd's path,
> the pruner will delete the labelled-but-no-longer-in-Git resources on the next
> pass — taking out resources Argo still needs.

## 6. Phase 1 — hand off one namespace

Hand off `app-b` first (the simpler case: no CR, no hook). The order of
operations enforces the one-owner rule.

**Step 1 — stop Argo auto-sync** so it will not prune `app-b` when its manifests
leave `argocd-managed/`:

```sh
argocd app set demo --sync-policy none
```

**Step 2 — move `app-b` into leancd's path** (Argo no longer sees it; with
auto-sync off it will not prune it):

```sh
cd manifests
mkdir -p leancd-managed/namespaces
git mv argocd-managed/namespaces/app-b leancd-managed/namespaces/app-b
git commit -qm "Phase 1: move app-b to leancd-managed" && git push
```

**Step 3 — let leancd take ownership.** Its next poll is a first run for
`app-b`'s resources (a HEAD change triggers `full=true`); to do it now:

```sh
kubectl -n leancd exec deploy/leancd -- leancd sync
# reconciliation complete ... managed=2 ...
```

**Step 4 — verify the handoff:**

```sh
kubectl get cm cm-b -n app-b -o jsonpath='{.metadata.labels}'
# {"app.kubernetes.io/managed-by":"leancd"}
kubectl get cm cm-b -n app-b -o jsonpath='{.metadata.managedFields[*].manager}'
# argocd-controller leancd   (Argo's entry lingers until §9 — harmless)
```

Confirm drift self-heal now works under leancd:

```sh
kubectl patch cm cm-b -n app-b -p '{"data":{"greeting":"HACKED"}}'
kubectl -n leancd exec deploy/leancd -- leancd sync   # logs: drift detected
kubectl get cm cm-b -n app-b -o jsonpath='{.data.greeting}'   # hello-from-argo-b
```

And confirm leancd is **not** touching `app-a`, which is still Argo's:

```sh
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.labels}'   # {} (no managed-by)
```

## 7. Phase 2 — hand off the rest (app-a + CRD + CR)

Repeat for `app-a`, the cluster-scoped CRD, its CR, and the hooks:

```sh
cd manifests
git mv argocd-managed/namespaces/app-a leancd-managed/namespaces/app-a
git mv argocd-managed/cluster        leancd-managed/cluster
git mv argocd-managed/hooks          leancd-managed/hooks
git commit -qm "Phase 2: move app-a, cluster (CRD+CR), hooks to leancd-managed" && git push
kubectl -n leancd exec deploy/leancd -- leancd sync
# reconciliation complete ... managed=9 ...
```

Argo CD's `argocd-managed/` is now empty; the `demo` Application shows
`OutOfSync` but, with auto-sync off, prunes nothing.

**CRD + CR note.** Here the CRD already existed (Argo created it), so leancd
takes both the CRD and its CR over in a single pass. If leancd ever has to
*create* a CRD from scratch, the CR whose CRD is not yet established is skipped
(non-fatal) and lands on the **second** `leancd sync` once discovery sees the
new CRD. Trigger a second pass if needed:

```sh
kubectl -n leancd exec deploy/leancd -- leancd sync
kubectl get leancdtest crd-test -n app-a    # present
```

## 8. Convert Argo CD hooks

leancd does not read `argocd.argoproj.io/hook` or `sync-wave`. Under leancd the
hook Jobs from [§4](#4-reproduce-the-argo-cd-managed-starting-state) are applied
as ordinary resources — no PreSync/PostSync ordering, no completion wait, no
auto-delete. To keep hook behaviour, convert to `helm.sh/hook`, which leancd
honours with Argo-CD-equivalent semantics (`hooks.rs`, `user-manual.md` §2):

| Argo CD | leancd (`helm.sh/hook`) |
|---|---|
| `argocd.argoproj.io/hook: PreSync` | `helm.sh/hook: pre-install` |
| `argocd.argoproj.io/hook: PostSync` | `helm.sh/hook: post-install` |
| `argocd.argoproj.io/hook-delete-policy: HookSucceeded` | `helm.sh/hook-delete-policy: hook-succeeded` |
| `argocd.argoproj.io/sync-wave: "N"` | `helm.sh/hook-weight: "N"` |

> **No leancd equivalent for `SyncFail` or cross-phase `sync-wave`.** `hook-weight`
> orders within a phase only. If a hook depends on these, decide case by case.

`leancd-managed/hooks/presync-job.yaml` becomes:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: presync-hook
  namespace: app-a
  annotations:
    helm.sh/hook: pre-install
    helm.sh/hook-weight: "0"
    helm.sh/hook-delete-policy: before-hook-creation
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: hook
          image: leancd:latest
          imagePullPolicy: IfNotPresent
          command: ["/bin/sh", "-c"]
          args: ["echo presync-hook-ran; exit 0"]
```

(analogously `postsync-job.yaml` with `helm.sh/hook: post-install`). Commit,
push, sync, and confirm leancd now runs them as hooks — pre-install before the
main apply, post-install after, each awaited to completion:

```sh
git commit -am "convert Argo hooks to helm.sh/hook" && git push
kubectl -n leancd exec deploy/leancd -- leancd sync
# logs: running helm hook phase=PreSync name=presync-hook ...
#       running helm hook phase=PostSync name=postsync-hook ...
# reconciliation complete ... managed=7 ...   (hooks no longer in main set)
kubectl get events -n app-a --sort-by=.lastTimestamp | tail
# Job/presync-hook Completed, then main apply, then Job/postsync-hook Completed
```

## 9. Remove Argo CD

Once leancd owns every resource, retire Argo CD without destroying them.

**Disarm the cascade finalizer first**, then delete the Application (orphan
delete — Argo's deletion will not cascade):

```sh
kubectl patch app demo -n argocd -p '{"metadata":{"finalizers":null}}' --type=merge
kubectl delete app demo -n argocd
# Verify leancd-managed resources SURVIVE:
kubectl get ns app-a app-b
kubectl get leancdtest -A
kubectl get cm cm-a -n app-a -o jsonpath='{.metadata.labels}'   # managed-by=leancd kept
```

Then uninstall Argo CD itself (its CRDs are gone, but our `e2e.leancd` CRD is
unrelated and survives):

```sh
kubectl delete -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml
kubectl delete ns argocd
kubectl get ns argocd                       # NotFound
kubectl get crd | grep argoproj || true     # (empty)
```

## 10. Verify the migration succeeded

```sh
# Every managed resource carries the leancd label
kubectl get cm,deploy -n app-a -l app.kubernetes.io/managed-by=leancd --show-labels
kubectl get cm -n app-b -l app.kubernetes.io/managed-by=leancd --show-labels

# leancd is healthy
kubectl -n leancd exec deploy/leancd -- leancd status     # managed=N, drift=0, no error
kubectl -n leancd exec deploy/leancd -- leancd health     # exit 0 (Fresh)

# Drift self-heals
kubectl patch cm cm-a -n app-a -p '{"data":{"greeting":"X"}}'
kubectl -n leancd exec deploy/leancd -- leancd sync       # drift detected; re-applying
kubectl get cm cm-a -n app-a -o jsonpath='{.data.greeting}'

# Prune works (remove a manifest, sync, resource is deleted; restore to undo)
git rm leancd-managed/namespaces/app-a/deployment.yaml && git commit -qm drop && git push
kubectl -n leancd exec deploy/leancd -- leancd sync       # pruned=1
git checkout HEAD~1 -- leancd-managed/namespaces/app-a/deployment.yaml
git commit -qm restore && git push
kubectl -n leancd exec deploy/leancd -- leancd sync       # resource recreated
```

> **Cosmetic note:** after removing Argo CD you may still see an
> `argocd-controller` entry in some resources' `metadata.managedFields`. It is
> inert — Argo CD is gone and will never write again — and leancd owns the fields
> that matter (`.force()` reclaimed them). It does not affect drift or prune. A
> full clean-up requires recreating the resource (brief downtime); it is purely
> cosmetic and can be ignored.

## 11. Rollback / abort

Before Argo CD is removed ([§9](#9-remove-argo-cd)), you can hand a namespace
back: move its manifests from `leancd-managed/` to `argocd-managed/`, re-enable
Argo auto-sync (`argocd app set demo --sync-policy automated`), and `argocd app
sync demo`. Argo's force-apply reclaims the fields. The `managed-by=leancd`
label leancd stamped will linger unless the resource is recreated — it is
harmless to Argo (Argo does not read it) but, if you want it gone, delete and
let Argo recreate the resource.

**Do not** roll back by simply narrowing leancd's path: the pruner will delete
the labelled resources that left the path (the [§5](#5-optional-observe-why-co-management-is-forbidden)
trap).

## 12. Clean up

```sh
kind delete cluster --name migration
```

## 13. Further reading

- [`./user-manual.md`](./user-manual.md) — full leancd reference (Helm hooks,
  prune, SSA, metrics).
- [`./tutorial.md`](./tutorial.md) — first-time deploy into a kind cluster.
- [`./architecture.md`](./architecture.md) — why leancd's prune, drift, and SSA
  are shaped the way they are.
- [`../README.md`](../README.md) — project overview.
