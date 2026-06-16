# Tutorial: deploy leancd into a kind cluster

This is a hands-on, ~20-minute walkthrough. You will build the leancd image,
load it into a local `kind` cluster, point it at a Git repository, and watch it
sync, drift-heal, and prune.

leancd runs **in-cluster as a Deployment** in this tutorial — the same shape you
would use in production, just on a local cluster.

> For the complete feature reference (every flag, env var, metric) see
> [`./user-manual.md`](./user-manual.md); for how it works internally see
> [`./architecture.md`](./architecture.md); for the project overview see
> [`../README.md`](../README.md).

## 0. What you will build

```
 your host                         kind node (container)
 ┌──────────────────┐              ┌──────────────────────────────────┐
 │ docker, kind,    │ build+load   │ leancd Pod ──► Kubernetes API    │
 │ kubectl, git     │ ───────────► │   └─► git clone/pull ──► Git     │
 └──────────────────┘              │ Forgejo Pod (Git)  ◄─ in-cluster │
                                   └──────────────────────────────────┘
```

leancd runs as a Pod, polls a Git repository, and applies the manifests it finds
there into the same cluster. In step 4 you choose where that Git repository
lives: an existing public repo, or a self-hosted Forgejo running inside the
cluster.

## 1. Prerequisites

You need these on your PATH:

| Tool | Why | Check |
|---|---|---|
| `docker` | build the leancd image | `docker --version` |
| `kind` | the local cluster | `kind version` |
| `kubectl` | talk to the cluster | `kubectl version --client` |
| `git` | prepare a repository | `git --version` |
| `curl` | create a repo / read metrics | `curl --version` |
| `cargo` | (optional) build the binary directly | `cargo --version` |

> The repository's Nix flake devShell provides `kind`, `kubectl`, and `curl`:
> run `nix develop` (or `direnv allow`) to get them. `docker` and `cargo` are
> still your responsibility.

## 2. Create a kind cluster

```sh
kind create cluster --name leancd-tutorial
kubectl get nodes   # expect one Ready node
```

`kind` writes a kubeconfig to `~/.kube/config`. Once leancd is deployed it uses
its in-cluster `ServiceAccount` (no kubeconfig needed inside the Pod).

## 3. Build and load the leancd image

```sh
docker build -t leancd:latest .
kind load docker-image leancd:latest --name leancd-tutorial
```

**Do not skip the `kind load`.** The Deployment uses `imagePullPolicy:
IfNotPresent`; if the image is not loaded into the kind node, the Pod will
`CrashLoopBackOff` with `ImagePullBackOff`/`ErrImagePull`.

## 4. Prepare a Git repository

leancd needs a Git repository containing one or more `*.yaml`/`*.yml` manifests.
Pick one of the three options below. **4a is the simplest**; **4c is fully
self-contained** (no external hosting) and mirrors what the e2e test suite does.

### 4a. A public hosted repository (simplest)

Use any public Git host (GitHub, GitLab, etc.). Put a manifest in it, e.g.
`manifests/cm.yaml`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: leancd-demo
  namespace: default
data:
  greeting: hello-from-git
```

Commit and push, then note the HTTPS clone URL (e.g.
`https://github.com/<you>/manifests.git`). Skip the credential Secret in step 5.

### 4b. A local `file://` repository (host-side only)

```sh
mkdir -p ~/leancd-manifests && cd ~/leancd-manifests
git init -q -b main
cat > cm.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: leancd-demo
  namespace: default
data:
  greeting: hello-from-git
EOF
git add -A && git -c user.email=d@d -c user.name=demo commit -qm "demo"
```

The URL is `file:///home/<you>/leancd-manifests`. **Caveat:** the path must be
reachable from the leancd Pod. A host path is *not* visible inside a container,
so `file://` only works when leancd runs as a host process (this is how the RSS
benchmark runs it). For an in-cluster Deployment, use **4a** or **4c**.

### 4c. An in-cluster Forgejo (fully self-contained)

This deploys [Forgejo](https://forgejo.org/) into the cluster as your Git
server — exactly what the e2e suite does. No external account needed.

Deploy Forgejo and wait for it:

```sh
kubectl apply -f tests/forgejo.yaml
kubectl wait -n forgejo --for=condition=Available deploy/forgejo --timeout=240s
```

Create the admin user (Forgejo ships with `INSTALL_LOCK=true`, so there is no
web wizard — the admin is created via the CLI inside the Pod):

```sh
kubectl -n forgejo exec deploy/forgejo -- \
  su -c "forgejo admin user create --admin \
    --username leancd --password leancd-e2e-pass \
    --email leancd@test.local --must-change-password=false \
    -c /data/gitea/conf/app.ini -w /var/lib/gitea" git
```

In a **second terminal**, port-forward the Forgejo HTTP port to your host:

```sh
kubectl -n forgejo port-forward svc/forgejo 3000:3000
```

Create a repository via the API (basic auth with the admin you just created),
auto-initialised on `main` so you can clone it immediately:

```sh
curl -sS -X POST -u leancd:leancd-e2e-pass \
  -H "Content-Type: application/json" \
  -d '{"name":"manifests","private":true,"auto_init":true,"default_branch":"main"}' \
  http://127.0.0.1:3000/api/v1/user/repos
```

Clone it, add a manifest, and push (username `leancd`, password
`leancd-e2e-pass`):

```sh
git clone http://127.0.0.1:3000/leancd/manifests.git
cd manifests
cat > cm.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: leancd-demo
  namespace: default
data:
  greeting: hello-from-git
EOF
git add -A && git -c user.email=d@d -c user.name=demo commit -qm "demo"
git push    # username: leancd, password: leancd-e2e-pass
```

From inside the cluster, leancd reaches this repo at
`http://forgejo.forgejo.svc.cluster.local:3000/leancd/manifests.git`. Keep the
`port-forward` terminal open if you want to keep pushing from your host; leancd
itself talks to the in-cluster Service URL, not `127.0.0.1`.

## 5. Create the Git credentials Secret (skip for public repos)

If your repository needs credentials, create the Secret leancd reads via
`envFrom`. The variable names (`GIT_USERNAME`/`GIT_PASSWORD` for HTTPS,
`GIT_SSH_KEY` for SSH) are the defaults; see
[`./user-manual.md` §6](./user-manual.md) for the full picture.

For the Forgejo repo from 4c:

```sh
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-literal=GIT_USERNAME=leancd \
  --from-literal=GIT_PASSWORD=leancd-e2e-pass
```

For an SSH repo:

```sh
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-file=GIT_SSH_KEY=$HOME/.ssh/id_ed25519
```

> The `leancd` namespace does not exist yet — it is created by
> `deploy/leancd.yaml` in step 6. Create it first so the Secret can land in it:
>
> ```sh
> kubectl create namespace leancd
> ```

## 6. Apply the leancd manifests

Edit [`../deploy/leancd.yaml`](../deploy/leancd.yaml) and set `LEANCD_REPO_URL`
(and `LEANCD_BRANCH`/`LEANCD_PATH` if needed) to your repository. For the
Forgejo repo:

```yaml
- name: LEANCD_REPO_URL
  value: "http://forgejo.forgejo.svc.cluster.local:3000/leancd/manifests.git"
```

Then apply and wait for the Deployment to become available:

```sh
kubectl apply -f deploy/leancd.yaml
kubectl wait -n leancd --for=condition=Available deploy/leancd --timeout=240s
```

If `wait` times out, check the Pod: `kubectl -n leancd describe pod -l
app.kubernetes.io/name=leancd` and `kubectl -n leancd logs deploy/leancd`. The
two common causes are a missing image (you skipped `kind load` in step 3) and a
bad repo URL or missing credentials.

If you created the credentials Secret *after* the first apply, restart the Pod
so it reads the new environment (a Pod reads `envFrom` only at startup):

```sh
kubectl -n leancd rollout restart deploy/leancd
```

## 7. Watch it sync

```sh
kubectl -n leancd logs deploy/leancd -f
```

Within one poll interval (default `60s`) you should see
`reconciliation complete` with `managed=1`. Check the recorded state:

```sh
kubectl -n leancd exec deploy/leancd -- leancd status
```

Confirm the manifest landed in the cluster:

```sh
kubectl get configmap leancd-demo -o yaml
```

## 8. Push a change and watch Git drive sync

Edit the manifest in your repository, commit, and push (for Forgejo, push
through the port-forward from step 4c). On the next poll, leancd detects the
moved HEAD (`full=true`) and re-applies. The logs show the new SHA.

To trigger a pass immediately instead of waiting:

```sh
kubectl -n leancd exec deploy/leancd -- leancd sync
```

## 9. Demonstrate drift self-heal

Change a field on the live resource directly, bypassing Git:

```sh
kubectl edit configmap leancd-demo   # change a value and save
```

On the next steady-state pass (HEAD unchanged), leancd lists the live resource,
sees it no longer matches Git (`spec_subset` fails), and re-applies. The logs
report `drift detected; re-applying managed resources`, and the field reverts to
the Git value.

## 10. Demonstrate prune

Remove the manifest from your repository, commit, and push. On the next pass
leancd notices the resource is gone from Git and deletes it:

```
pruned resource no longer in Git ...
```

```sh
kubectl get configmap leancd-demo   # NotFound
```

## 11. Inspect metrics

Port-forward the metrics Service and read `/metrics`:

```sh
# in a second terminal:
kubectl -n leancd port-forward svc/leancd-metrics 9090:9090

# then:
curl -s localhost:9090/metrics | grep leancd_
```

Look for:

- `leancd_rss_bytes` — the headline metric; it must stay under 100MiB.
- `leancd_managed_resources` — number of Git-managed resources.
- `leancd_sync_total` / `leancd_sync_errors_total` — pass and error counts.
- `leancd_drift_detected{group,version,kind}` — drifts found last pass.

See [`./user-manual.md` §7](./user-manual.md) for the full metric table and
PromQL examples.

## 12. (Optional) Benchmark RSS

The RSS budget is verified by an automated benchmark that runs the same
in-cluster code paths as a host process against a kind cluster:

```sh
make bench        # single run at the default scale (200 resources)
make scale        # RSS across 100/300/500 resources
```

This is not required to use leancd, but it verifies the ≤ 100MiB guarantee on
your machine. See [`../bench/README.md`](../bench/README.md).

## 13. Clean up

```sh
kind delete cluster --name leancd-tutorial
```

Deleting the cluster removes everything in it, including the in-cluster Forgejo
from step 4c (its data is on an `emptyDir`, so nothing persists).

## 14. Next steps

- [`./user-manual.md`](./user-manual.md) — every flag, tuning, troubleshooting.
- [`./architecture.md`](./architecture.md) — how reconciliation, drift, and
  prune actually work, and why leancd is shaped this way.
- For production: narrow the `ClusterRole`, pick a real Git host, and set
  resource limits appropriate to your scale.
