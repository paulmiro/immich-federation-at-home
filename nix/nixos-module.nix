{ self, ... }:
let
  name = "immich-federation-at-home";
in
{
  flake.nixosModules.default =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.${name};

      # systemd wants strings. `toString true` is "1", which the program does not accept for
      # RUN_ONCE (clap only takes the literal "true"/"false"), so booleans are spelled out.
      toEnvValue = value: if lib.isBool value then lib.boolToString value else toString value;
    in
    {
      options.services.${name} = {
        enable = lib.mkEnableOption "the immich-federation-at-home album mirror";

        package = lib.mkOption {
          type = lib.types.package;
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          defaultText = lib.literalMD "the `default` package of the immich-federation-at-home flake";
          description = "Package providing the `immich-federation-at-home` binary.";
        };

        settings = lib.mkOption {
          type =
            with lib.types;
            attrsOf (oneOf [
              str
              int
              bool
            ]);
          default = { };
          example = {
            EXPORT_ALBUM_URL = "https://photos.friend.example/share/AbC123";
            IMPORT_SERVER_URL = "https://immich.example.com";
            IMPORT_ALBUM = "Family Photos";
            IMPORT_INTERVAL = "1h";
          };
          description = ''
            Environment variables for the service.
            `CACHE_DIR` is already set correctly.

            Do NOT put secrets here, use {option}`services.${name}.environmentFile` instead.
          '';
        };

        environmentFile = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/run/secrets/immich-federation-at-home.env";
          description = "Path to a systemd `EnvironmentFile` holding at least `IMPORT_API_KEY=…`.";
        };
      };

      config = lib.mkIf cfg.enable {
        warnings =
          lib.optional (cfg.settings ? IMPORT_API_KEY || cfg.settings ? EXPORT_ALBUM_PASSWORD)
            "services.${name}.settings contains a secret, which puts it in the world-readable Nix store. Use services.${name}.environmentFile instead.";

        systemd.services.${name} = {
          description = "Mirror a shared Immich album into a local album";
          wantedBy = [ "multi-user.target" ];
          # The program contacts both instances at startup and exits if it cannot reach them,
          # so starting before there is a route just burns a restart cycle.
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];

          environment = {
            CACHE_DIR = "/var/cache/${name}";
          }
          // lib.mapAttrs (lib.const toEnvValue) cfg.settings;

          serviceConfig = {
            ExecStart = lib.getExe cfg.package;
            EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;

            DynamicUser = true;
            CacheDirectory = name;

            Restart = "on-failure";
            RestartSec = 60;

            # Each asset is staged in a temporary file at full size before being re-uploaded,
            # so PrivateTmp (implied by DynamicUser) needs room for the largest asset in the
            # album. Point TMPDIR elsewhere via `settings` if /tmp is a small tmpfs.

            AmbientCapabilities = [ "" ];
            CapabilityBoundingSet = [ "" ];
            LockPersonality = true;
            MemoryDenyWriteExecute = true;
            NoNewPrivileges = true;
            PrivateDevices = true;
            ProtectClock = true;
            ProtectControlGroups = true;
            ProtectHome = true;
            ProtectHostname = true;
            ProtectKernelLogs = true;
            ProtectKernelModules = true;
            ProtectKernelTunables = true;
            ProtectProc = "invisible";
            RestrictAddressFamilies = [
              "AF_INET"
              "AF_INET6"
              # DNS through nss-resolved/nscd goes over a unix socket.
              "AF_UNIX"
            ];
            RestrictNamespaces = true;
            RestrictRealtime = true;
            RestrictSUIDSGID = true;
            SystemCallArchitectures = "native";
            SystemCallFilter = [
              "@system-service"
              "~@privileged"
              "~@resources"
            ];
          };
        };
      };
    };
}
