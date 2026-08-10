{
  description = "immich-federation-at-home — mirror a shared Immich album into a local album";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs@{ flake-parts, crane, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        {
          pkgs,
          lib,
          self',
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

          # craneLib.cleanCargoSource keeps only Rust/cargo files, which would silently drop
          # openapi/immich-openapi-3.1.0.json (read at runtime by tests/spec_conformance.rs
          # via env!("CARGO_MANIFEST_DIR")) and the e2e fixtures/compose file. Keep them
          # explicitly.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              (craneLib.fileset.commonCargoSources ./.)
              ./openapi
              ./tests/e2e/fixtures
              ./tests/e2e/compose.yaml
            ];
          };

          commonArgs = {
            inherit src;
            strictDeps = true;

            pname = "immich-federation-at-home";
            version = "0.1.0";
          };

          # Deps built once, reused by the package build and every check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          immich-federation-at-home = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              # checks.nextest already runs the test suite; running it twice is pure latency.
              doCheck = false;
            }
          );

          # apps.update-openapi (task 14 will add scripts/update-openapi.sh; until it exists,
          # this is a self-contained writeShellApplication that does the fetch itself so
          # `nix flake check`/`nix run .#update-openapi` work today. Task 14 should either
          # delete this and point apps.update-openapi at the new script, or have the script
          # just be this body — whichever is cleaner at that point.)
          update-openapi = pkgs.writeShellApplication {
            name = "update-openapi";
            runtimeInputs = [ pkgs.curl ];
            text = ''
              out="$(git rev-parse --show-toplevel)/openapi/immich-openapi-3.1.0.json"
              curl -fsSL -o "$out" https://docs.immich.app/openapi.json
              echo "updated $out"
            '';
          };
        in
        {
          packages.default = immich-federation-at-home;

          packages.docker = pkgs.dockerTools.buildLayeredImage {
            name = "immich-federation-at-home";
            tag = "latest";
            contents = [
              pkgs.cacert
              pkgs.dockerTools.fakeNss
            ];
            extraCommands = ''
              mkdir -p tmp
              chmod 1777 tmp
            '';
            config = {
              Entrypoint = [ "${self'.packages.default}/bin/immich-federation-at-home" ];
              Env = [ "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" ];
              User = "65534:65534";
            };
          };

          checks = {
            inherit immich-federation-at-home;

            cargoClippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            cargoFmt = craneLib.cargoFmt { inherit src; };

            cargoNextest = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                # nextest skips #[ignore]d tests by default, so the e2e suite (needs Docker
                # and network) stays out of `nix flake check`.
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
                # reqwest's rustls-platform-verifier reads the OS cert store at runtime; the
                # build sandbox has no /etc/ssl/certs, so any test that constructs a client
                # (even one that only ever talks to 127.0.0.1) fails with "No CA certificates
                # were loaded from the system" unless SSL_CERT_FILE points somewhere real.
                SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
              }
            );
          };

          devShells.default = craneLib.devShell {
            checks = self'.checks;
            packages = with pkgs; [
              rust-analyzer
              clippy
              rustfmt
              jq
              curl
              docker-compose
            ];
          };

          apps.update-openapi = {
            program = "${update-openapi}/bin/update-openapi";
            meta.description = "Refresh openapi/immich-openapi-3.1.0.json from upstream";
          };

          # `pkgs.nixfmt-rfc-style` is now an alias for `pkgs.nixfmt` and warns on evaluation.
          formatter = pkgs.nixfmt;
        };
    };
}
