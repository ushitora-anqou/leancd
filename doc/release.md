# Release process

How to cut a Lean CD release. A single `vX.Y.Z` tag publishes the container
image, the Helm chart, and a GitHub Release — all at the same version — via
[`.github/workflows/release.yml`](../.github/workflows/release.yml). The manual
work is one command: `make release`.

## 1. Write the changelog

Add the release's entries under `[Unreleased]` in
[`CHANGELOG.md`](../CHANGELOG.md). `make release` does the mechanical part —
moving `[Unreleased]` under a dated `[X.Y.Z]` heading — but it does not write
the entries themselves.

## 2. `make release`

[`make release`](../Makefile) runs [`scripts/release.sh`](../scripts/release.sh),
which:

1. bumps the patch version across `Cargo.toml` and `Chart.yaml` (`version` +
   `appVersion`) — `nix flake check`'s `chart-version-consistency` fails if the
   three diverge;
2. moves the `[Unreleased]` section in `CHANGELOG.md` under a new
   `[X.Y.Z] - YYYY-MM-DD` heading and prepends an empty `[Unreleased]`;
3. runs the full local gate (`make fmt` + `make test` == `nix flake check`);
4. commits (`chore(release): vX.Y.Z`), tags (`git tag -a vX.Y.Z`), and pushes
   `master` and the tag.

The push triggers the release workflow (below). The script refuses to run
unless you are on `master`, the working tree is clean, and `HEAD` matches
`origin/master`.

Preview the bump without committing/tagging/pushing:

```sh
RELEASE_DRYRUN=1 make release
```

## 3. The release workflow builds and publishes

Pushing a `v*` tag triggers three jobs:

- **build-and-push** — builds the `linux/amd64` image (Buildx), embeds the
  exact git SHA via `--build-arg GIT_SHA=$(git rev-parse --short=8 HEAD)`
  (see `build.rs`) so `leancd --version` is correct even though `.git` is
  excluded, and pushes `ghcr.io/ushitora-anqou/leancd:X.Y.Z`, `:X.Y`, and
  `:latest`.
- **chart** — runs in parallel with the image build (no data dependency);
  packages the chart (`helm package --version X.Y.Z --app-version X.Y.Z`),
  lints the tarball, and pushes it to GHCR as an OCI artifact at
  `oci://ghcr.io/ushitora-anqou/charts/leancd` — a package distinct from the
  image. OCI tags are immutable, so the job pulls first and skips the push when
  `X.Y.Z` is already published (idempotent re-runs of a failed job).
- **release** — runs after both; creates the GitHub Release with notes extracted
  from the `CHANGELOG.md` section for `X.Y.Z` (falling back to auto-generated
  notes) and attaches the chart `.tgz`.

Watch the run:

```sh
gh run watch
```

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
