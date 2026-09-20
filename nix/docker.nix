{
  perSystem =
    {
      pkgs,
      lib,
      self',
      packageFor,
      ...
    }:
    let
      version = self'.packages.default.version;
      imageName = "ghcr.io/paulmiro/immich-federation-at-home";

      # Everything in the image comes from `targetPkgs`, including the binary itself and
      # the `architecture` field dockerTools takes from its host platform, so image and
      # binary always agree — and all of it is *cross*-compiled from the machine running
      # the build, which therefore never has to be able to execute the target's code.
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
            Entrypoint = [ (lib.getExe (packageFor targetPkgs)) ];
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
    in
    {
      # `.#docker` is the image for the machine you are on — the one to `docker load`
      # locally. `.#docker-amd64`/`.#docker-arm64` name their architecture explicitly and
      # are what gets pushed, so a release is the same two images from any builder.
      packages.docker = imageFor pkgs;
      packages.docker-amd64 = images.amd64;
      packages.docker-arm64 = images.arm64;

      apps.docker-push = {
        meta.description = "Push the Docker image to ghcr.io";
        program = pkgs.writeShellApplication {
          name = "docker-push";
          runtimeInputs = [
            pkgs.skopeo
            pkgs.regclient
          ];
          text = ''
            ${lib.concatStrings (
              lib.mapAttrsToList (arch: image: ''
                echo "pushing ${imageName}:${version}-${arch}"
                skopeo copy "docker-archive:${image}" "docker://${imageName}:${version}-${arch}" "$@"
              '') images
            )}
            for tag in "${version}" latest; do
              echo "indexing ${imageName}:$tag"
              regctl index create "${imageName}:$tag" \
                ${lib.concatMapStringsSep " " (arch: ''--ref "${imageName}:${version}-${arch}"'') (
                  lib.attrNames images
                )}
            done
          '';
        };
      };
    };
}
