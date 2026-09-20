{
  inputs,
  ...
}:
{
  perSystem =
    {
      pkgs,
      lib,
      ...
    }:
    let
      # The native toolchain: what `checks` and the dev shell run. The images below
      # instantiate crane once more per target architecture.
      craneLib = inputs.crane.mkLib pkgs;

      # craneLib.cleanCargoSource keeps only Rust/cargo files, which would silently drop
      # openapi/immich-openapi.json (read at runtime by tests/spec_conformance.rs
      # via env!("CARGO_MANIFEST_DIR")) and the e2e fixtures/compose file. Keep them
      # explicitly.
      src = lib.fileset.toSource {
        root = ../.;
        fileset = lib.fileset.unions [
          (craneLib.fileset.commonCargoSources ../.)
          ../openapi
          ../tests/e2e/fixtures
          ../tests/e2e/compose.yaml
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
        version = "0.3.1";

        meta.mainProgram = "immich-federation-at-home";
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
          craneLib' = inputs.crane.mkLib targetPkgs;
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
    in
    {
      # Handed to nix/docker.nix so an image can build *its own* binary for the
      # architecture it is for. Reaching for `self.packages.<that system>.default`
      # instead would pick the natively-built package of a foreign system, which needs
      # a builder (or binfmt emulation) for that architecture — the whole point of
      # going through pkgsCross is not to need one.
      _module.args.packageFor = packageFor;

      packages.default = immich-federation-at-home;

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
    };
}
