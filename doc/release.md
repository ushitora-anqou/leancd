# Release process

How to cut a Lean CD release. A single `vX.Y.Z` tag publishes the container
image, the Helm chart, and a GitHub Release — all at the same version — via
[`.github/workflows/release.yml`](../.github/workflows/release.yml). The steps
below are the manual parts around it.

## 1. Prepare the release

- Move the `[Unreleased]` entries in [`CHANGELOG.md`](../CHANGELOG.md) under a
  new `[X.Y.Z] - YYYY-MM-DD` heading.
- Bump the version to the same `X.Y.Z` in three places (`nix flake check`'s
  `chart-version-consistency` fails if they diverge):
  - `version = "..."` in [`Cargo.toml`](../Cargo.toml),
  - `version:` in [`charts/leancd/Chart.yaml`](../charts/leancd/Chart.yaml),
  - `appVersion:` in the same `Chart.yaml` — this is the image tag the chart
    resolves to by default (see `charts/leancd/templates/deployment.yaml`).
- Run the full local gate:

  ```sh
  make fmt
  make test        # nix flake check: fmt + clippy -D + nextest + deny + audit
                   #   + helm lint/template + chart-version-consistency
  make bench       # optional: confirm RSS stays within budget
  ```

## 2. Tag and push

```sh
git tag -s vX.Y.Z -m "leancd vX.Y.Z"
git push origin vX.Y.Z
```

## 3. The release workflow builds and publishes

Pushing a `v*` tag triggers three jobs:

- **build-and-push** — builds `linux/amd64` and `linux/arm64` images (QEMU +
  Buildx), embeds the exact git SHA via
  `--build-arg GIT_SHA=$(git rev-parse --short=8 HEAD)` (see `build.rs`) so
  `leancd --version` is correct even though `.git` is excluded, and pushes
  `ghcr.io/ushitora-anqou/leancd:X.Y.Z`, `:X.Y`, and `:latest`.
- **chart** — runs in parallel with the image build (no data dependency);
  packages the chart (`helm package --version X.Y.Z --app-version X.Y.Z`), lints
  the tarball, and pushes it to GHCR as an OCI artifact at
  `oci://ghcr.io/ushitora-anqou/charts/leancd` — a package distinct from the
  image. OCI tags are immutable, so the job pulls first and skips the push when
  `X.Y.Z` is already published (idempotent re-runs of a failed job).
- **release** — runs after both; creates the GitHub Release with notes extracted
  from the `CHANGELOG.md` section for `X.Y.Z` (falling back to auto-generated
  notes) and attaches the chart `.tgz`.

Watch the runs under the repository's **Actions** tab.

## 4. First-time GHCR package setup (once)

The `ghcr.io/ushitora-anqou/charts` package is created by the workflow on the
first release (under the tag-pusher's namespace). Once it exists, open its
**Package settings** and:

- change the visibility to **Public** (so the chart installs without
  authentication), and
- link this repository under **Manage Actions access** (so future
  `GITHUB_TOKEN` pushes inherit the repo's permissions).

This is a manual one-time step that cannot be done from the workflow.

## Consuming the image

```sh
docker pull ghcr.io/ushitora-anqou/leancd:X.Y.Z
```

`leancd --version` in the image reports `X.Y.Z (sha …)`.

## Consuming the chart

Install the published OCI chart directly — OCI needs no `helm repo add`:

```sh
helm install leancd oci://ghcr.io/ushitora-anqou/charts/leancd \
  --version X.Y.Z \
  --namespace leancd --create-namespace \
  --set config.repoUrl=<your repo>
```

The chart's default image is `ghcr.io/ushitora-anqou/leancd` at
`Chart.appVersion` (`X.Y.Z`), so you don't set `image.*` for the published
build. Override `image.tag` to pin a different version, or `image.repository`
for a mirror.

Upgrade to a later release:

```sh
helm upgrade leancd oci://ghcr.io/ushitora-anqou/charts/leancd \
  --version <new X.Y.Z> --namespace leancd
```
