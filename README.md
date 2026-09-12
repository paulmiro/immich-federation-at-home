# immich-federation-at-home

(it's just as good as the real thing, trust me)

## 100% vibe-coded slop, but works for me

`immich-federation-at-home` periodically mirrors a shared album from someone else's
("export") Immich instance — one you have no account on, only a public share link for —
into an album on your own ("import") instance, authenticated with an API key. It's
**one-way and additive**: assets flow export → import only, and nothing is ever deleted or
modified on either side.

The export instance must be running **Immich v3.0.3 or newer**.

## Setup

On the **export** instance, share the album as a link (Album → Share → Create link) with **Allow public user to download** turned **on**.
A password is optional, you can pass it as `EXPORT_ALBUM_PASSWORD`.
Copy the link (either the `/share/<key>` or the short `/s/<slug>` form) into `EXPORT_ALBUM_URL`.
If the link has an expiry, the program warns when it's within 7 days and stops working once it passes.

On the **import** instance, create the target album (it is **not** created for you) and an API key (Account Settings → API Keys) with these permissions:
- `asset.upload`
- `album.read` 
- `albumAsset.create`

## Docker Compose

```yaml
services:
  immich-federation-at-home:
    image: ghcr.io/paulmiro/immich-federation-at-home:latest
    restart: unless-stopped
    environment:
      # Share link for the album on the *other* person's instance, and its password if any.
      EXPORT_ALBUM_URL: "https://photos.friend.example/share/AbC123"
      # EXPORT_ALBUM_PASSWORD: "..."

      # Your own instance and an API key with the permissions listed above.
      IMPORT_SERVER_URL: "https://immich.example.com"
      IMPORT_API_KEY: "..."
      # UUID or exact name of an *existing* album on your instance.
      IMPORT_ALBUM: "Family Photos"

      INTERVAL: "1h"
      TRANSFER_CONCURRENCY: "4"
      LOG_LEVEL: "info"
    tmpfs:
      # Assets are staged here TRANSFER_CONCURRENCY at a time, so size this for that many
      # times the largest single asset, not the album total. Drop it to stage on disk instead.
      - /tmp:size=8g
    volumes:
      # Strongly recommended. Without it the cache is discarded every time the container is
      # re-created, which can mean re-downloading a lot if the source album contains
      # external-library assets. Use a named volume — a bind mount needs to be
      # `chown 65532:65532`ed first or startup fails.
      - cache:/cache

volumes:
  cache:
```

### Multiple jobs

Point `CONFIG_FILE` at a mounted config file instead of the per-job variables above. A
top-level `configs:` block with inline `content:` keeps everything in one `compose.yaml`,
but needs Compose ≥ 2.23:

```yaml
configs:
  jobs:
    content: |
      import_server_url  = "https://immich.example.com"
      import_api_key_env = "IMPORT_API_KEY"

      [jobs.family]
      export_album_url = "https://their-immich.example.com/s/some-shared-album"
      import_album     = "Family Photos"

services:
  immich-federation-at-home:
    image: ghcr.io/paulmiro/immich-federation-at-home:latest
    restart: unless-stopped
    configs:
      - source: jobs
        target: /config.toml
    environment:
      CONFIG_FILE: /config.toml
      IMPORT_API_KEY: "..."   # referenced from the file via import_api_key_env
    tmpfs:
      # TRANSFER_CONCURRENCY times the largest single asset, across every job.
      - /tmp:size=8g
    volumes:
      - cache:/cache

volumes:
  cache:
```

Below Compose 2.23, use `CONFIG` with a YAML block scalar instead of `configs:`:

```yaml
    environment:
      CONFIG: |
        import_server_url  = "https://immich.example.com"
        import_api_key_env = "IMPORT_API_KEY"

        [jobs.family]
        export_album_url = "https://their-immich.example.com/s/some-shared-album"
        import_album     = "Family Photos"
      IMPORT_API_KEY: "..."
```

See [Running several jobs in one process](#running-several-jobs-in-one-process) for the
full config format, including secrets and precedence.

## NixOS module

```nix
{
  imports = [ inputs.immich-federation-at-home.nixosModules.default ];

  services.immich-federation-at-home = {
    enable = true;

    # Any of the environment variables below, except the secrets.
    settings = {
      EXPORT_ALBUM_URL = "https://photos.friend.example/share/AbC123";
      IMPORT_SERVER_URL = "https://immich.example.com";
      IMPORT_ALBUM = "Family Photos";
      INTERVAL = "1h";
    };

    # IMPORT_API_KEY=… and, if the share link has one, EXPORT_ALBUM_PASSWORD=….
    environmentFile = "/run/secrets/immich-federation-at-home.env";
  };
}
```

For several jobs, or a secret read from a file instead of an environment file, set
`services.immich-federation-at-home.jobs.<name>` instead — see that option's own
description for the full per-job key list.

## Running the binary directly

```sh
nix run github:paulmiro/immich-federation-at-home -- --help
nix build github:paulmiro/immich-federation-at-home   # ./result/bin/…
```

The program loops on its own, so a plain service unit is enough:

```ini
# /etc/systemd/system/immich-federation-at-home.service
[Unit]
Description=Mirror a shared Immich album
After=network-online.target
Wants=network-online.target

[Service]
User=immich-federation
EnvironmentFile=/etc/immich-federation-at-home.env   # chmod 600, holds the variables below
ExecStart=/usr/local/bin/immich-federation-at-home
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Prefer a timer? Set `RUN_ONCE=true`, make the unit `Type=oneshot`, and drive it with a `.timer`.

## Environment variables

Every variable has an equivalent `--kebab-case-flag` that has priority over the environment
variable, with two exceptions: `CONFIG_FILE`'s flag is `--config`, and `CONFIG` (inline TOML)
has no flag at all — it exists for platforms that can only inject environment variables.
Variables with no Default value (-) are required (after inheritance, if a config file is in
play — see [Running several jobs in one process](#running-several-jobs-in-one-process)).

Process-global — describe the process, not any one job:

| Variable               | Default | Meaning                                                                                        |
| ---------------------- | ------- | ---------------------------------------------------------------------------------------------- |
| `LOG_LEVEL`            | `info`  | `error\|warn\|info\|debug\|trace`. `RUST_LOG` is not read.                                     |
| `CACHE_DIR`            | unset   | Where to keep the content-hash cache. Startup fails if it is set but not writable.             |
| `TMPDIR`               | system  | Where assets are staged while in flight. Needs room for `TRANSFER_CONCURRENCY` of them.        |
| `TRANSFER_CONCURRENCY` | `4`     | How many assets are transferred in parallel, across every job in the process.                  |
| `CONFIG_FILE`          | unset   | Path to a TOML config file. Mutually exclusive with `CONFIG`.                                  |
| `CONFIG`               | unset   | The TOML config inline, for environment-only platforms. Mutually exclusive with `CONFIG_FILE`. |

Per-job — used directly when there is no config file, describing the single implicit job;
inside a config file the same keys, lowercased, go in a `[jobs.<name>]` table or at the top
level as a default for every job:

| Variable                | Default | Meaning                                                                                 |
| ----------------------- | ------- | ---------------------------------------------------------------------------------------- |
| `EXPORT_ALBUM_URL`      | -       | Share link for the album to mirror. Sub-path deployments and trailing slashes are fine. |
| `EXPORT_ALBUM_PASSWORD` | unset   | Password for the share link, if it has one.                                             |
| `IMPORT_SERVER_URL`     | -       | Your own instance, e.g. `https://immich.example.com`. A trailing `/` or `/api` is fine. |
| `IMPORT_API_KEY`        | -       | API key for the import instance; see [Setup](#setup).                                   |
| `IMPORT_ALBUM`          | -       | Target album: a UUID, or an exact album name. It must already exist.                    |
| `INTERVAL`              | `1h`    | How often to check for new assets (`30m`, `1h30m`, `6h`, …).                            |
| `REQUEST_TIMEOUT`       | `30s`   | Timeout for metadata calls.                                                             |
| `TRANSFER_TIMEOUT`      | `30m`   | Timeout for transferring a single asset.                                                |

`RUN_ONCE`/`--once` (`true`/`false` only, default `false`) does one pass over every job and
exits, non-zero if any job failed. It's an invocation mode for the whole process, not a
config-file key — it applies even when `--config` is set.

## Running several jobs in one process

With no `--config` / `CONFIG_FILE` / `CONFIG`, the environment variables above describe
exactly one job, named `default` in the logs — today's behaviour, unchanged. Every existing
deployment keeps working untouched.

Point one at a config file to run more than one job. `--config` / `CONFIG_FILE` takes a
path; `CONFIG` takes the TOML inline, for platforms that can only inject environment
variables (older Compose, Portainer, Kubernetes). Setting both is a startup error. There is
no implicit config path — no file, one job, always.

Once a config file is in play, **it is the whole job list**: the per-job environment
variables stop reaching into jobs entirely (the process-global ones still apply, see
precedence below). Jobs live in `[jobs.<name>]` tables; any job key set at the top level
becomes the default for every job that doesn't set its own. Job names only ever show up in
logs.

```toml
transfer_concurrency = 4

# Defaults for every job below.
import_server_url   = "https://my-immich.example.com"
import_api_key_file = "/run/secrets/immich-api-key"
interval            = "1h"

[jobs.family]
export_album_url           = "https://their-immich.example.com/s/some-shared-album"
export_album_password_file = "/run/secrets/family-link-password"
import_album               = "Family Photos"

[jobs.hiking]
export_album_url = "https://their-other-immich.example.com/s/some-shared-album"
interval         = "12h"           # slow server, don't hammer it
import_album     = "Family Photos" # same album as `family`, deliberately
```

**Precedence.** Process-global keys: flag > file > env > default. Job keys: job table >
top-level default > built-in default — environment variables never reach into jobs once a
file exists.

**Secrets.** `import_api_key` and `export_album_password` each also accept a `*_file`
variant (a path, read at startup) and a `*_env` variant (the name of an environment
variable, read at startup) — exactly one spelling per key per job. `*_file` is what makes
Docker secrets, systemd `LoadCredential`, sops-nix and agenix work. Startup warns if a
config file holding an inline secret is group- or world-readable.

**Merging albums.** Two jobs may deliberately target the same `import_album` — that's the
supported way to merge several source albums into one.

**Concurrency.** `transfer_concurrency` is process-wide; there is no per-job knob. A job
that needs to be gentle with a slow export server uses a longer `interval` instead.

**Failure isolation.** One job failing its startup checks (an expired share link, say)
doesn't stop the others — it's retried on that job's next tick. The one exception: if
*every* job fails its very first attempt, the process exits non-zero at startup instead of
looping forever with nothing working (this is also what a single job with no config file
does today, unchanged). With `RUN_ONCE`, every job runs once and the process exits non-zero
if any job failed.

## What gets synced

Every run lists the source album and asks your instance which of those checksums it already
has. Only the missing ones are transferred; all of them are then added to the target album.
Nothing is remembered between runs, so there is no database to back up and no state to
corrupt — but permanently deleting an imported asset means the next run brings it back.
(Assets in your trash are recognised and left alone.)

The exception is assets that live in an **external library** on the export side. Immich
identifies those by their path rather than their contents, so their checksum is useless for
this and the program has to download one once to learn what it really is. `CACHE_DIR` is
where it remembers that, which is why losing the cache can cost a large re-download. Albums
without external-library assets are unaffected either way.

## Known limitations

* **Live photos**: the motion-video half is skipped. Embedded motion photos
  (Pixel/Samsung `.MP.jpg`) survive anyway, because the still's own bytes contain the video;
  a separately uploaded video (iPhone) is not transferred.
* **Additive only**: deletions and album removals on the source are never mirrored.
* **Deleted assets come back** on the next run. Ask your friend to remove it from the shared album.
* **No sidecars**: XMP files cannot be fetched via the Immich API. Embedded EXIF metadata survives.

## Troubleshooting

* **A download fails with `400`.**: "Allow public user to download" is off on the share link.
* **Startup lists missing permissions.** Grant them on the `IMPORT_API_KEY`.
* **`import album … does not exist`.** `IMPORT_ALBUM` is read as a UUID first, then as an
  *exact* name; the error lists the albums that do exist. Create it first — it is never
  auto-created. If several albums share the name, use a UUID.
* **Startup names the export server's version.** It is older than v3.0.3. The only fix is
  upgrading the server (which you really should be doing anyways).
* **A `401` at startup.** Wrong `EXPORT_ALBUM_PASSWORD`.
* **The share link expired.** Ask its owner for a new one and update `EXPORT_ALBUM_URL`.
* **An asset is skipped as `unsupported format`.** Your instance rejected it on upload; it won't be retried.
* **`CACHE_DIR` can't be created or written.** A permissions problem, fatal on purpose.
  Use a named Docker volume, or `chown 65532:65532` the bind-mounted directory.
  Deleting `CACHE_DIR` from your compose file doesn't disable the cache, because the image sets it.
* **The cache is empty after every deploy.** Add the named volume from the [Docker Compose](#docker-compose) snippet.
