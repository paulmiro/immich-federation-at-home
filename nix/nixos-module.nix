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

      optionalStr = description: {
        type = lib.types.nullOr lib.types.str;
        default = null;
        inherit description;
      };

      # Every option here is named after the config-file key it becomes (snake_case, not
      # Nix's usual camelCase): `settings` is rendered straight to TOML, so keeping the two
      # spellings identical means the option and the file it produces are obviously the same
      # thing, with no translation table to keep in your head.
      #
      # Used for the two secrets (`export_album_password`, `import_api_key`) and, since
      # they're an identical shape (one key, plus a `_file`/`_env` alternative that keeps it
      # out of the world-readable Nix store), the two URL keys as well — a domain name isn't
      # a credential, but some users still don't want it sitting in `settings`.
      fileEnvOptions = optName: description: {
        ${optName} = lib.mkOption (optionalStr ''
          ${description} Puts it in the world-readable Nix store — prefer
          `${optName}_file` or `${optName}_env`.
        '');
        "${optName}_file" = lib.mkOption (optionalStr ''
          Path to read `${optName}` from at startup (trailing newline trimmed). Point
          it at a systemd `LoadCredential` (`%d/…`) or a sops-nix/agenix path. A plain
          string, not a Nix path, so a `%d/…` specifier isn't misread as a store path.
        '');
        "${optName}_env" = lib.mkOption (
          optionalStr "Name of an environment variable holding `${optName}`, read at startup."
        );
      };

      # Shared by one job and by the top level, where the same keys act as the default for
      # every job that does not set them itself.
      jobOptions = {
        import_album = lib.mkOption (
          optionalStr "Target album: a UUID, or an exact, already-existing album name."
        );
        interval = lib.mkOption (
          optionalStr "How often to check for new assets (`30m`, `1h30m`, `6h`, …). Default: `1h`."
        );
        request_timeout = lib.mkOption (optionalStr "Timeout for metadata calls. Default: `30s`.");
        transfer_timeout = lib.mkOption (
          optionalStr "Timeout for transferring a single asset. Default: `30m`."
        );
        tags = lib.mkOption {
          type = lib.types.nullOr (lib.types.listOf lib.types.str);
          default = null;
          description = ''
            Tags to attach to every synced asset, created on the import instance if they
            don't already exist. Unlike every other option here, a job's own list is
            *merged with* (never overridden by) the one set next to {option}`jobs`, deduplicated.
          '';
        };
      }
      // fileEnvOptions "export_album_url" "Share link for the album to mirror."
      // fileEnvOptions "import_server_url" "Your own instance, e.g. `https://immich.example.com`."
      // fileEnvOptions "export_album_password" "Password for the share link, if it has one."
      // fileEnvOptions "import_api_key" "API key for the import instance.";

      settingsModule = lib.types.submodule {
        options = jobOptions // {
          log_level = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.enum [
                "error"
                "warn"
                "info"
                "debug"
                "trace"
              ]
            );
            default = null;
            description = "Log verbosity. Default: `info`.";
          };

          cache_dir = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = "/var/cache/${name}";
            description = ''
              Directory for the content-hash cache, shared by every job. `null` disables
              persisting it, so every restart re-checksums whatever it downloads.
            '';
          };

          tmp_dir = lib.mkOption (optionalStr ''
            Directory used to stage each asset while it is in flight. Set this if the
            service's `/tmp` is a tmpfs too small for `transfer_concurrency` times the
            largest single asset across every job.
          '');

          transfer_concurrency = lib.mkOption {
            type = lib.types.nullOr lib.types.ints.positive;
            default = null;
            description = ''
              How many assets to transfer at once, across every job in the process.
              Default: `4`.
            '';
          };

          jobs = lib.mkOption {
            type = lib.types.attrsOf (lib.types.submodule { options = jobOptions; });
            default = { };
            description = ''
              Jobs to run, keyed by a name that only ever shows up in logs. At least one is
              required. A key left unset here falls back to the same key set next to
              {option}`jobs`, which is how several jobs share one import instance and API
              key. Inheritance is per key, not per spelling: a job that sets any one of
              `import_api_key`, `_file` or `_env` ignores all three of the inherited ones.
            '';
          };
        };
      };

      # `null` means "not set", and is dropped rather than rendered, so the program's own
      # default (or its required-key error) applies.
      dropNulls = lib.filterAttrs (_: v: v != null);

      tomlFormat = pkgs.formats.toml { };
      configFile = tomlFormat.generate "${name}-config.toml" (
        dropNulls (builtins.removeAttrs cfg.settings [ "jobs" ])
        // {
          jobs = lib.mapAttrs (_: dropNulls) cfg.settings.jobs;
        }
      );

      # A job's own spelling of a three-spelling key wins over the inherited one as a set of
      # three, so "is it set here" is asked of a whole table, never of a single spelling.
      secretSpellings =
        table: key:
        lib.count (v: v != null) [
          table.${key}
          table."${key}_file"
          table."${key}_env"
        ];
      inheritedValue = job: key: if job.${key} != null then job.${key} else cfg.settings.${key};
      hasSecret = job: key: secretSpellings job key > 0 || secretSpellings cfg.settings key > 0;

      # The two actual secrets — the only keys `warnings` below flags for landing inline in
      # the world-readable Nix store. The URL keys accept the same `_file`/`_env` spellings
      # (below, `multiSpellingKeys`) but aren't credentials, so using them inline is never
      # warned about.
      secretKeys = [
        "export_album_password"
        "import_api_key"
      ];

      # Every key that accepts the `key`/`key_file`/`key_env` trio — the secrets, plus the
      # two URL keys some users want to keep out of `settings` for privacy rather than
      # secrecy.
      multiSpellingKeys = secretKeys ++ [
        "export_album_url"
        "import_server_url"
      ];

      jobAssertions =
        jobName: job:
        map
          (key: {
            assertion = hasSecret job key;
            message =
              "services.${name}.settings.jobs.${jobName} has no ${key}, ${key}_file or "
              + "${key}_env, and neither does services.${name}.settings.";
          })
          [
            "export_album_url"
            "import_server_url"
            "import_api_key"
          ]
        ++ [
          {
            assertion = inheritedValue job "import_album" != null;
            message =
              "services.${name}.settings.jobs.${jobName}.import_album is not set, and neither "
              + "is the default in services.${name}.settings.import_album.";
          }
        ]
        ++ map (key: {
          assertion = secretSpellings job key <= 1;
          message =
            "services.${name}.settings.jobs.${jobName} sets ${key} more than once — use only one "
            + "of ${key}, ${key}_file or ${key}_env.";
        }) multiSpellingKeys;
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
          type = settingsModule;
          example = {
            import_server_url = "https://immich.example.com";
            import_api_key_file = "%d/immich-api-key";
            jobs = {
              family = {
                export_album_url = "https://their-immich.example.com/s/some-shared-album";
                export_album_password_file = "%d/family-link-password";
                import_album = "Family Photos";
              };
              hiking = {
                export_album_url = "https://their-other-immich.example.com/s/some-shared-album";
                import_album = "Family Photos"; # same album as `family`, deliberately
                interval = "12h"; # slow server, don't hammer it
              };
            };
          };
          description = ''
            The program's configuration file, rendered to TOML and passed to the service.
            Options are named after the file's own keys, so `nix eval` on this option and
            the file the service reads say the same thing.

            Keys set next to {option}`jobs` are the default for every job that does not set
            them itself.

            Secrets — and, for those who'd rather not have a domain name sitting in the Nix
            store either, {option}`export_album_url`/{option}`import_server_url` — put here
            directly land in the world-readable Nix store: prefer the `_file` spelling of
            each (pointing at a systemd `LoadCredential` or a sops-nix/agenix path), or
            `_env` together with {option}`services.${name}.environmentFile`.
          '';
        };

        environmentFile = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/run/secrets/immich-federation-at-home.env";
          description = ''
            Path to a systemd `EnvironmentFile`. Holds the variables named by any
            `_env` secret key in {option}`services.${name}.settings`.
          '';
        };
      };

      config = lib.mkIf cfg.enable {
        assertions = [
          {
            assertion = cfg.settings.jobs != { };
            message = "services.${name}.settings.jobs is empty: define at least one job to run.";
          }
        ]
        ++ map (key: {
          assertion = secretSpellings cfg.settings key <= 1;
          message =
            "services.${name}.settings sets ${key} more than once — use only one of ${key}, "
            + "${key}_file or ${key}_env.";
        }) multiSpellingKeys
        ++ lib.concatLists (lib.mapAttrsToList jobAssertions cfg.settings.jobs);

        warnings =
          let
            inlineIn = table: lib.any (key: table.${key} != null) secretKeys;
          in
          lib.optional (inlineIn cfg.settings || lib.any inlineIn (lib.attrValues cfg.settings.jobs))
            "services.${name}.settings contains an inline secret, which puts it in the world-readable Nix store. Use the _file or _env spelling of that key instead.";

        systemd.services.${name} = {
          description = "Mirror a shared Immich album into a local album";
          wantedBy = [ "multi-user.target" ];
          # The program contacts both instances at startup and exits if it cannot reach them,
          # so starting before there is a route just burns a restart cycle.
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];

          environment.CONFIG_FILE = "${configFile}";

          serviceConfig = {
            ExecStart = lib.getExe cfg.package;
            EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;

            DynamicUser = true;
            CacheDirectory = name;

            Restart = "on-failure";
            RestartSec = 60;

            # Each in-flight transfer stages one asset at full size before re-uploading it, so
            # PrivateTmp (implied by DynamicUser) needs room for transfer_concurrency times
            # the largest single asset across every job, not the album total. Point
            # `settings.tmp_dir` elsewhere if /tmp is a small tmpfs.

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
