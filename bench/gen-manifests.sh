#!/usr/bin/env bash
# Generate N Kubernetes manifests into a directory for RSS benchmarking.
# Usage: gen-manifests.sh [COUNT] [OUT_DIR]
set -euo pipefail

COUNT="${1:-200}"
OUT="${2:-manifests}"
mkdir -p "$OUT"

# Mix of resource kinds to exercise discovery across groups/scope.
for i in $(seq 1 "$COUNT"); do
  cat > "$OUT/cm-$i.yaml" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: leancd-bench-cm-$i
  namespace: default
data:
  index: "$i"
  payload: "$(printf 'x%.0s' $(seq 1 200))"
EOF
done

# A couple of cluster-scoped resources to cover the Scope::Cluster path.
cat > "$OUT/namespace.yaml" <<'EOF'
apiVersion: v1
kind: Namespace
metadata:
  name: leancd-bench
EOF

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

echo "generated $COUNT ConfigMaps (+2 cluster-scoped) into $OUT"
