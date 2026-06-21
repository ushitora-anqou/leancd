# Release process

How to cut a leancd release. The container image is published to GHCR by a
GitHub Actions workflow on every `v*` tag; the steps below are the manual
parts around it.

## 1. Prepare the release

- Move the `[Unreleased]` entries in [`CHANGELOG.md`](../CHANGELOG.md) under a
  new `[X.Y.Z] - YYYY-MM-DD` heading.
- Bump `version = "..."` in [`Cargo.toml`](../Cargo.toml) if it has diverged
  from the version you intend to ship.
- Run the full local gate:

  ```sh
  make fmt
  make test        # nix flake check: fmt + clippy -D + nextest + deny + audit
  make bench       # optional: confirm RSS stays within budget
  ```

## 2. Tag and push

```sh
git tag -s vX.Y.Z -m "leancd vX.Y.Z"
git push origin vX.Y.Z
```

## 3. The release workflow builds and publishes

Pushing a `v*` tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml),
which:

- builds `linux/amd64` and `linux/arm64` images (QEMU + Buildx),
- embeds the exact git SHA via
  `--build-arg GIT_SHA=$(git rev-parse --short=8 HEAD)` (see `build.rs`), so
  `leancd --version` in the image is correct even though `.git` is excluded,
- pushes `ghcr.io/<owner>/leancd:X.Y.Z`, `:X.Y`, and `:latest`.

Watch the run under the repository's **Actions** tab.

## 4. Publish release notes

On GitHub **Releases**, create a release for the tag and paste the
`CHANGELOG.md` section for that version.

## Consuming the image

```sh
docker pull ghcr.io/<owner>/leancd:X.Y.Z
```

`<owner>` is the GitHub repository owner in **lowercase** (GHCR requires
lowercase). Point `deploy/leancd.yaml`'s `image:` at this reference instead of
the local `leancd:latest`.
