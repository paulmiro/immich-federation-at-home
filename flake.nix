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
          # The native toolchain: what `checks` and the dev shell run. The images below
          # instantiate crane once more per target architecture.
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

          # crane drives cargo directly instead of through nixpkgs' Rust hooks, so the target
          # triple and the linker to use for it have to be spelled out: without them a cross
          # build would happily emit host binaries and then fail to link. `targetPkgs` is a
          # package set whose *host* platform is the one the binary will run on — `pkgs` itself
          # for a native build, `pkgs.pkgsCross.<x>` to cross-compile.
          cargoTargetArgs = targetPkgs: {
            CARGO_BUILD_TARGET = targetPkgs.stdenv.hostPlatform.rust.rustcTarget;
            "CARGO_TARGET_${targetPkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_LINKER" =
              "${targetPkgs.stdenv.cc.targetPrefix}cc";

            # Build scripts (proc-macro crates, and the C in aws-lc-sys that rustls pulls in)
            # are compiled for and run on the build machine, so they need an *unprefixed*
            # native compiler: with strictDeps the cross `cc` is the only one on PATH, and
            # rustc fails with "linker `cc` not found". Natively this is the compiler that is
            # already there, so it changes nothing.
            depsBuildBuild = [ targetPkgs.pkgsBuildBuild.stdenv.cc ];
          };

          commonArgs = {
            inherit src;
            strictDeps = true;

            pname = "immich-federation-at-home";
            version = "0.1.0";
          }
          // cargoTargetArgs pkgs;

          # Deps built once, reused by the package build and every check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # The binary, built for `targetPkgs`' host platform. For `pkgs` this is the same
          # derivation `cargoArtifacts` above is shared with, because the arguments come out
          # identical — nothing is compiled twice to keep one code path for both cases.
          packageFor =
            targetPkgs:
            let
              craneLib' = crane.mkLib targetPkgs;
              args = commonArgs // cargoTargetArgs targetPkgs;
            in
            craneLib'.buildPackage (
              args
              // {
                cargoArtifacts = craneLib'.buildDepsOnly args;
                # checks.nextest already runs the test suite; running it twice is pure latency.
                doCheck = false;
              }
            );

          immich-federation-at-home = packageFor pkgs;

          version = self'.packages.default.version;
          imageName = "ghcr.io/paulmiro/immich-federation-at-home";

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

          # Everything in the image comes from `targetPkgs`, including the `architecture` field
          # dockerTools takes from its host platform, so image and binary always agree.
          imageFor =
            targetPkgs:
            targetPkgs.dockerTools.buildLayeredImage {
              name = imageName;
              tag = version;
              contents = [
                targetPkgs.cacert
                targetPkgs.dockerTools.fakeNss
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
                mkdir -p cache
                chown -R 65532:65532 cache
              '';
              config = {
                Entrypoint = [ "${packageFor targetPkgs}/bin/immich-federation-at-home" ];
                Env = [
                  "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
                  "CACHE_DIR=/cache"
                ];
                User = "65532:65532";
              };
            };

          # The architectures a release covers, keyed by the name the registry knows them
          # under. `pkgsCross.gnu64` is only nominally a cross build on x86-64 — nixpkgs
          # collapses a crossSystem equal to the build system back to the native stdenv, so
          # this is the very same derivation `packages.default` is, not a second compile.
          images = {
            amd64 = imageFor pkgs.pkgsCross.gnu64;
            arm64 = imageFor pkgs.pkgsCross.aarch64-multiplatform;
          };

          # Each architecture is pushed once under a tag of its own: an image index can only
          # point at manifests that already exist in the registry, and skopeo has no way to push
          # one by digest alone. Both real tags are then published as an OCI index over those
          # same two manifests, which is what makes a plain `docker pull` resolve to the right
          # architecture. Extra arguments go to skopeo only — regctl takes different flags, and
          # both read the same credentials by default.
          pushArch = arch: image: ''
            echo "pushing ${imageName}:${version}-${arch}"
            skopeo copy "docker-archive:${image}" "docker://${imageName}:${version}-${arch}" "$@"
          '';

          docker-push = pkgs.writeShellApplication {
            name = "docker-push";
            runtimeInputs = [
              pkgs.skopeo
              pkgs.regclient
            ];
            text = ''
              ${lib.concatStrings (lib.mapAttrsToList pushArch images)}
              for tag in "${version}" latest; do
                echo "indexing ${imageName}:$tag"
                regctl index create "${imageName}:$tag" \
                  ${lib.concatMapStringsSep " " (arch: ''--ref "${imageName}:${version}-${arch}"'') (
                    lib.attrNames images
                  )}
              done
            '';
          };
        in
        {
          packages.default = immich-federation-at-home;

          # `.#docker` is the image for the machine you are on — the one to `docker load`
          # locally. `.#docker-amd64`/`.#docker-arm64` name their architecture explicitly and
          # are what gets pushed, so a release is the same two images from any builder.
          packages.docker = imageFor pkgs;
          packages.docker-amd64 = images.amd64;
          packages.docker-arm64 = images.arm64;

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
              secretspec
            ];
          };

          apps.update-openapi = {
            program = "${update-openapi}/bin/update-openapi";
            meta.description = "Refresh openapi/immich-openapi-3.1.0.json from upstream";
          };

          apps.docker-push = {
            program = "${docker-push}/bin/docker-push";
            meta.description = "Push the Docker image to ghcr.io";
          };

          # `pkgs.nixfmt-rfc-style` is now an alias for `pkgs.nixfmt` and warns on evaluation.
          formatter = pkgs.nixfmt;
        };
    };
}
