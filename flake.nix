{
  description = "Build a cargo project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    crane,
    flake-utils,
    advisory-db,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        inherit (pkgs) lib;

        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;

          buildInputs =
            [
              # Add additional build inputs here
            ]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              # Additional darwin specific inputs can be set here
              pkgs.libiconv
            ];

          # Additional environment variables can be set directly
          # MY_CUSTOM_VAR = "some value";
        };

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the actual crate itself, reusing the dependency
        # artifacts from above.
        my-crate = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );

        # Helm chart sources. craneLib.cleanCargoSource (used for `src` above)
        # keeps only Cargo-related files, so gather charts/ separately by
        # extension — the same helper my-crate-toml-fmt uses for .toml. The
        # .json picks up the Grafana dashboard definition.
        chartSrc = pkgs.lib.sourceFilesBySuffices ./charts [".yaml" ".yml" ".json" ".tpl" ".txt"];
      in {
        formatter = pkgs.alejandra;

        checks = {
          # Build the crate as part of `nix flake check` for convenience
          inherit my-crate;

          # Run clippy (and deny all warnings) on the crate source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
          my-crate-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          my-crate-doc = craneLib.cargoDoc (
            commonArgs
            // {
              inherit cargoArtifacts;
              # This can be commented out or tweaked as necessary, e.g. set to
              # `--deny rustdoc::broken-intra-doc-links` to only enforce that lint
              env.RUSTDOCFLAGS = "--deny warnings";
            }
          );

          # Check formatting
          my-crate-fmt = craneLib.cargoFmt {
            inherit src;
          };

          my-crate-toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [".toml"];
            # taplo arguments can be further customized below as needed
            # taploExtraArgs = "--config ./taplo.toml";
          };

          # Audit dependencies
          my-crate-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          # Audit licenses
          my-crate-deny = craneLib.cargoDeny {
            inherit src;
          };

          # Run tests with cargo-nextest
          # Consider setting `doCheck = false` on `my-crate` if you do not want
          # the tests to run twice
          my-crate-nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );

          # Lint the Helm chart (validates Chart.yaml, values.schema.json, and
          # that every template renders without errors).
          helm-lint = pkgs.runCommand "helm-lint" {
            nativeBuildInputs = [pkgs.kubernetes-helm];
          } ''
            cp -r ${chartSrc} charts
            chmod -R u+w charts
            helm lint charts/leancd
            touch $out
          '';

          # Structure-test the chart: render several value variations with
          # `helm template` and assert the expected resources/labels/env are
          # present or absent. Uses only helm + grep (no extra plugin).
          helm-template = pkgs.runCommand "helm-template" {
            nativeBuildInputs = [pkgs.kubernetes-helm];
          } ''
            cp -r ${chartSrc} charts
            chmod -R u+w charts

            # (1) Default cluster posture.
            helm template leancd charts/leancd > cluster.yaml
            grep -q "kind: Deployment" cluster.yaml
            grep -q "kind: ClusterRoleBinding" cluster.yaml
            grep -q "kind: Namespace" cluster.yaml
            if grep -q "kind: NetworkPolicy" cluster.yaml; then
              echo "unexpected NetworkPolicy in cluster mode" >&2
              exit 1
            fi
            # Default image tracks Chart.AppVersion from the published GHCR repo,
            # with no explicit image.tag (see deployment.yaml). Read appVersion
            # dynamically so this stays correct across version bumps; escape the
            # dots so a hypothetical X.Y.Z-rc1 can't falsely match.
            appver=$(grep -E '^appVersion:' charts/leancd/Chart.yaml | awk '{print $2}' | tr -d '"')
            appver_re=$(printf '%s' "$appver" | sed 's/\./\\./g')
            grep -qE "image: \"ghcr.io/ushitora-anqou/leancd:$appver_re\"$" cluster.yaml

            # (2) Namespaced posture.
            helm template leancd charts/leancd \
              --set rbac.namespaced=true \
              --set networkPolicy.kubeApiCidr=172.16.0.0/16 > ns.yaml
            grep -q "kind: RoleBinding" ns.yaml
            grep -q "kind: NetworkPolicy" ns.yaml
            grep -q "172.16.0.0/16" ns.yaml
            if grep -q "kind: ClusterRoleBinding" ns.yaml; then
              echo "unexpected ClusterRoleBinding in namespaced mode" >&2
              exit 1
            fi

            # (3) Dashboard ConfigMap ships the JSON + the grafana_dashboard label.
            helm template leancd charts/leancd --set dashboards.enabled=true > dash.yaml
            grep -q "kind: ConfigMap" dash.yaml
            grep -q "grafana_dashboard" dash.yaml
            grep -q "leancd-overview" dash.yaml

            # (4) Dashboard disabled ships no ConfigMap.
            helm template leancd charts/leancd --set dashboards.enabled=false > nodash.yaml
            if grep -q "kind: ConfigMap" nodash.yaml; then
              echo "unexpected ConfigMap with dashboards disabled" >&2
              exit 1
            fi

            # (5) All LEANCD_*/OTEL_* envs and the credentials Secret are injected.
            helm template leancd charts/leancd \
              --set config.repoUrl=https://git.example.com/x.git > env.yaml
            grep -q "LEANCD_REPO_URL" env.yaml
            grep -q "LEANCD_LOCK_LEASE_DURATION_SECS" env.yaml
            grep -q "OTEL_EXPORTER_OTLP_ENDPOINT" env.yaml
            grep -q "https://git.example.com/x.git" env.yaml
            grep -q "leancd-git-credentials" env.yaml

            # (6) extraEnv override (last value wins).
            helm template leancd charts/leancd \
              --set-json 'extraEnv=[{"name":"OTEL_METRIC_EXPORT_INTERVAL","value":"5000"}]' > extra.yaml
            grep -q "OTEL_METRIC_EXPORT_INTERVAL" extra.yaml

            # (7) image.tag override still wins over Chart.AppVersion.
            helm template leancd charts/leancd --set image.tag=canary > img.yaml
            grep -qE 'image: "ghcr.io/ushitora-anqou/leancd:canary"$' img.yaml

            touch $out
          '';

          # Assert the chart version/appVersion match the Cargo package version,
          # so a half-bumped release (tag != chart != binary) is caught locally by
          # `nix flake check` before tag push. Pure grep/awk/tr — no extra deps.
          chart-version-consistency = pkgs.runCommand "chart-version-consistency" {} ''
            chart_ver=$(grep -E '^version:'   ${chartSrc}/leancd/Chart.yaml | awk '{print $2}')
            app_ver=$(grep -E '^appVersion:'  ${chartSrc}/leancd/Chart.yaml | awk '{print $2}' | tr -d '"')
            cargo_ver=$(grep -E '^version'    ${./Cargo.toml} | head -1 | awk -F'"' '{print $2}')
            echo "chart=$chart_ver appVersion=$app_ver cargo=$cargo_ver"
            [ "$chart_ver" = "$cargo_ver" ] || {
              echo "ERROR: Chart.yaml version ($chart_ver) != Cargo.toml version ($cargo_ver)" >&2
              exit 1
            }
            [ "$app_ver" = "$cargo_ver" ] || {
              echo "ERROR: Chart.yaml appVersion ($app_ver) != Cargo.toml version ($cargo_ver)" >&2
              exit 1
            }
            touch $out
          '';
        };

        packages = {
          default = my-crate;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = my-crate;
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          checks = self.checks.${system};

          # Additional dev-shell environment variables can be set directly
          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = with pkgs; [
            curl
            kind
            kubectl
            kubernetes-helm
          ];
        };
      }
    );
}
