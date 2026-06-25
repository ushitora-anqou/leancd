#!/usr/bin/env bash
# Generate a realistic, multi-namespace manifest set for RSS benchmarking.
#
# Mirrors a production cluster: N namespaces, each carrying a mix of
# Deployments, StatefulSets, ConfigMaps, and Services. Workloads run real
# (pause) Pods so the cluster state resembles a live environment while
# leancd's reconciliation (apply/list/prune/drift) is exercised across many
# GVKs and namespaces.
#
# Usage: gen-manifests.sh [NAMESPACE_COUNT] [OUT_DIR]
set -euo pipefail

NS_COUNT="${1:-15}"
OUT="${2:-manifests}"
mkdir -p "$OUT"

# Per-namespace workload composition (the "heavy" profile). Each is overridable
# via env (BENCH_DEP_PER_NS / BENCH_DEP_REPLICAS / BENCH_STS_PER_NS /
# BENCH_CM_PER_NS / BENCH_SVC_PER_NS) so the cache-bloat and health-heavy
# scenarios can scale object counts and per-Deployment Pod fan-out.
DEP_PER_NS="${BENCH_DEP_PER_NS:-5}"
# Replicas per Deployment: scales the ReplicaSet->Pod fan-out (the
# ownerReference chain) whose aggregate state is read each pass via the
# Deployment's .status during health assessment.
DEP_REPLICAS="${BENCH_DEP_REPLICAS:-2}"
STS_PER_NS="${BENCH_STS_PER_NS:-2}"
CM_PER_NS="${BENCH_CM_PER_NS:-8}"
SVC_PER_NS="${BENCH_SVC_PER_NS:-3}"

# Reused ConfigMap payload (generated once, not per-resource). Size is tunable
# via BENCH_PAYLOAD_BYTES (default 200) for the large-object cache-bloat scenario.
PAYLOAD_BYTES="${BENCH_PAYLOAD_BYTES:-200}"
PAYLOAD="$(head -c "$PAYLOAD_BYTES" /dev/zero | tr '\0' 'x')"

for n in $(seq 1 "$NS_COUNT"); do
  ns="leancd-bench-ns-$n"

  # Namespace itself (cluster-scoped).
  cat > "$OUT/namespace-$n.yaml" <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $ns
EOF

  # Deployments (real pause Pods).
  for i in $(seq 1 "$DEP_PER_NS"); do
    cat > "$OUT/deploy-${n}-${i}.yaml" <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app-deploy-$i
  namespace: $ns
spec:
  replicas: $DEP_REPLICAS
  selector:
    matchLabels:
      app: app-deploy-$i
  template:
    metadata:
      labels:
        app: app-deploy-$i
    spec:
      containers:
        - name: app
          image: registry.k8s.io/pause:3.9
EOF
  done

  # StatefulSets (real pause Pods; serviceName is a required field).
  for i in $(seq 1 "$STS_PER_NS"); do
    cat > "$OUT/sts-${n}-${i}.yaml" <<EOF
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: app-sts-$i
  namespace: $ns
spec:
  replicas: 2
  serviceName: app-sts-$i
  selector:
    matchLabels:
      app: app-sts-$i
  template:
    metadata:
      labels:
        app: app-sts-$i
    spec:
      containers:
        - name: app
          image: registry.k8s.io/pause:3.9
EOF
  done

  # ConfigMaps.
  for i in $(seq 1 "$CM_PER_NS"); do
    cat > "$OUT/cm-${n}-${i}.yaml" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: app-cm-$i
  namespace: $ns
data:
  index: "$i"
  payload: "$PAYLOAD"
EOF
  done

  # Services (ClusterIP, selecting the matching Deployment).
  for i in $(seq 1 "$SVC_PER_NS"); do
    cat > "$OUT/svc-${n}-${i}.yaml" <<EOF
apiVersion: v1
kind: Service
metadata:
  name: app-svc-$i
  namespace: $ns
spec:
  selector:
    app: app-deploy-$i
  ports:
    - port: 80
      targetPort: 80
EOF
  done
done

# A cluster-scoped resource to cover the Scope::Cluster path.
cat > "$OUT/clusterrole.yaml" <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: leancd-bench-view
rules:
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["get", "list"]
EOF

# Optionally merge every generated manifest into one multi-document YAML file
# (for the single-large-file RSS scenario). The per-resource files are removed.
if [ "${BENCH_MERGE_TO_SINGLE_FILE:-0}" = "1" ]; then
  merged="$OUT/all.yaml"
  first=1
  for f in "$OUT"/*.yaml; do
    [ "$f" = "$merged" ] && continue
    if [ "$first" = 1 ]; then
      cp "$f" "$merged"
      first=0
    else
      printf '\n---\n' >> "$merged"
      cat "$f" >> "$merged"
    fi
    rm -f "$f"
  done
fi

per_ns=$(( DEP_PER_NS + STS_PER_NS + CM_PER_NS + SVC_PER_NS ))
total=$(( NS_COUNT * per_ns ))
echo "generated $NS_COUNT namespaces x $per_ns resources each ($total namespaced + $NS_COUNT Namespaces + ClusterRole) into $OUT"
