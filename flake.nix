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

      imports = [
        ./nix/nixos-module.nix
        ./nix/docker.nix
        ./nix/package.nix
      ];

      perSystem =
        {
          pkgs,
          self',
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;
        in
        {
          formatter = pkgs.nixfmt;

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
            meta.description = "Refresh openapi/immich-openapi-3.1.0.json from upstream";

            program = pkgs.writeShellApplication {
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
          };
        };
    };
}
