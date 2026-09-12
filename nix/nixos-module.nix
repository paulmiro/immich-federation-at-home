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

      # Per-job options are named after the program's own TOML keys (snake_case) rather than
      # Nix's usual camelCase: this option is rendered straight into the config file, so
      # keeping the two spellings identical means the option and the TOML it produces are
      # obviously the same thing, with no translation table to keep in your head.
      secretOptions = secretName: {
        ${secretName} = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = ''
            ${secretName}, inline. Puts it in the world-readable Nix store — prefer
            `${secretName}_file` or `${secretName}_env`.
          '';
        };
        "${secretName}_file" = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = ''
            Path to read `${secretName}` from at startup (trailing newline trimmed). Point
            it at a systemd `LoadCredential` (`%d/…`) or a sops-nix/agenix path. A plain
            string, not a Nix path, so a `%d/…` specifier isn't misread as a store path.
          '';
        };
        "${secretName}_env" = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Name of an environment variable holding `${secretName}`, read at startup.";
        };
      };

      jobModule = lib.types.submodule {
        options = {
          export_album_url = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Share link for the album to mirror. Required.";
          };
          import_server_url = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Your own instance, e.g. `https://immich.example.com`. Required.";
          };
          import_album = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Target album: a UUID, or an exact, already-existing album name. Required.";
          };
          interval = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "How often to check for new assets (`30m`, `1h30m`, `6h`, …). Program default: `1h`.";
          };
          request_timeout = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Timeout for metadata calls. Program default: `30s`.";
          };
          transfer_timeout = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Timeout for transferring a single asset. Program default: `30m`.";
          };
        }
        // secretOptions "export_album_password"
        // secretOptions "import_api_key";
      };

      # `null` means "not set" and is dropped below, so the program's own default (or its
      # required-key error, for the fields with no default) applies -- this module doesn't
      # duplicate that validation.
      dropNulls = lib.filterAttrs (_: v: v != null);

      hasJobs = cfg.jobs != { };

      tomlFormat = pkgs.formats.toml { };
      configFile = tomlFormat.generate "${name}-config.toml" {
        jobs = lib.mapAttrs (_: dropNulls) cfg.jobs;
      };

      jobHasInlineSecret = lib.any (
        job: job.export_album_password != null || job.import_api_key != null
      ) (lib.attrValues cfg.jobs);
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
            INTERVAL = "1h";
          };
          description = ''
            Environment variables for the service. `CACHE_DIR` is already set correctly.

            These are the process-global keys (`LOG_LEVEL`, `TMPDIR`, `TRANSFER_CONCURRENCY`,
            …) and the escape hatch for anything {option}`services.${name}.jobs` does not
            model yet. A rendered config file's own keys beat the environment, but a global
            key left out of the file (which is every global key, since this module only ever
            renders `jobs`) falls back to `settings` exactly as before -- so `settings` keeps
            working as the way to set globals whether or not `jobs` is in use.

            Per-job keys set here (`EXPORT_ALBUM_URL`, `IMPORT_ALBUM`, …) are read only when
            {option}`services.${name}.jobs` is empty, where they describe the single implicit
            job; once `jobs` renders a config file, the program does not consult the
            environment for job keys at all, so these are silently ignored -- use
            {option}`services.${name}.jobs` instead.

            Do NOT put secrets here, use {option}`services.${name}.environmentFile` instead.
          '';
        };

        environmentFile = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/run/secrets/immich-federation-at-home.env";
          description = "Path to a systemd `EnvironmentFile` holding at least `IMPORT_API_KEY=…`.";
        };

        jobs = lib.mkOption {
          type = lib.types.attrsOf jobModule;
          default = { };
          example = {
            family = {
              export_album_url = "https://their-immich.example.com/s/some-shared-album";
              export_album_password_file = "%d/family-link-password";
              import_server_url = "https://immich.example.com";
              import_api_key_file = "%d/immich-api-key";
              import_album = "Family Photos";
            };
            hiking = {
              export_album_url = "https://their-other-immich.example.com/s/some-shared-album";
              import_server_url = "https://immich.example.com";
              import_api_key_file = "%d/immich-api-key";
              import_album = "Family Photos"; # same album as `family`, deliberately
              interval = "12h"; # slow server, don't hammer it
            };
          };
          description = ''
            Jobs to run, keyed by a name that only ever shows up in logs. Set at least one to
            render a config file at all; with `jobs` empty (the default), the service runs on
            {option}`services.${name}.settings` alone, exactly as before.

            There is no Nix-level equivalent of the config format's top-level default-for-
            every-job keys -- share values between jobs with ordinary Nix (a `let`-bound
            attrset merged into each job), the same way you would share any other option.

            Any job missing a required key (`export_album_url`, `import_server_url`, an API
            key, or `import_album`, after its own settings) fails at startup with an error
            naming the job and the key; this module does not duplicate that check.
          '';
        };
      };

      config = lib.mkIf cfg.enable {
        warnings =
          lib.optional
            (cfg.settings ? IMPORT_API_KEY || cfg.settings ? EXPORT_ALBUM_PASSWORD || jobHasInlineSecret)
            "services.${name}.settings or .jobs.<name> contains an inline secret, which puts it in the world-readable Nix store. Use services.${name}.jobs.<name>.import_api_key_file/_env and .export_album_password_file/_env, or services.${name}.environmentFile, instead.";

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
          // lib.optionalAttrs hasJobs { CONFIG_FILE = "${configFile}"; }
          // lib.mapAttrs (lib.const toEnvValue) cfg.settings;

          serviceConfig = {
            ExecStart = lib.getExe cfg.package;
            EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;

            DynamicUser = true;
            CacheDirectory = name;

            Restart = "on-failure";
            RestartSec = 60;

            # Each in-flight transfer stages one asset at full size before re-uploading it, so
            # PrivateTmp (implied by DynamicUser) needs room for TRANSFER_CONCURRENCY times
            # the largest single asset across every job, not the album total. Point TMPDIR
            # elsewhere via `settings` if /tmp is a small tmpfs.

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
