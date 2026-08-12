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

          # apps.update-openapi wraps scripts/update-openapi.sh, which is the single source
          # of truth: this just supplies its runtime deps (curl, jq, git) so `nix run
          # .#update-openapi` works on a machine with none of them on PATH, then execs the
          # real script unmodified so it and `./scripts/update-openapi.sh` can't drift.
          update-openapi = pkgs.writeShellApplication {
            name = "update-openapi";
            runtimeInputs = [
              pkgs.curl
              pkgs.jq
              pkgs.git
            ];
            text = ''
              exec "${./scripts/update-openapi.sh}" "$@"
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
            # The content-hash cache lives here. It has to be owned by the same uid the
            # image runs as, so that a *named* Docker volume mounted over it inherits that
            # ownership and just works; a bind mount does not, and the program says so
            # explicitly when it cannot write here. Without a volume this is the container's
            # writable layer, which is still useful — the cache survives for the life of the
            # container, just not across a re-create.
            #
            # This is `fakeRootCommands` rather than `extraCommands` because `chown` to
            # another uid is not permitted in the Nix build sandbox; fakeroot is what records
            # the ownership into the layer without actually needing the privilege.
            fakeRootCommands = ''
              mkdir -p var/cache/immich-federation-at-home
              chown -R 65532:65532 var/cache/immich-federation-at-home
            '';
            config = {
              Entrypoint = [ "${self'.packages.default}/bin/immich-federation-at-home" ];
              Env = [
                "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
                "CACHE_DIR=/var/cache/immich-federation-at-home"
              ];
              User = "65532:65532";
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
