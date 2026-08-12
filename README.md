# immich-federation-at-home

(it's just as good as the real thing, trust me)

`immich-federation-at-home` periodically mirrors a shared album from someone else's
("export") Immich instance — one you have no account on, only a public share link for —
into an album on your own ("import") instance, authenticated with an API key. It's
**one-way and additive**: assets flow export → import only, nothing is ever deleted or
modified on either side, and removing an asset from the source album does not remove it
from your copy.

## Contents

1. [API key permissions](#api-key-permissions)
2. [Docker Compose](#docker-compose)
3. [NixOS module](#nixos-module)
4. [Running the binary directly](#running-the-binary-directly)
5. [Environment variables](#environment-variables)
6. [Setting up the share link](#setting-up-the-share-link)
7. [Export instance version requirement](#export-instance-version-requirement)
8. [How deduplication works](#how-deduplication-works)
9. [Known limitations](#known-limitations)
10. [Secrets with secretspec](#secrets-with-secretspec)
11. [Troubleshooting](#troubleshooting)
12. [Development](#development)

## API key permissions

Create the `IMPORT_API_KEY` on your **import** instance (Account Settings → API Keys) with
at least these three permissions:

| Permission          | Unlocks                                                          |
| -------------------- | ----------------------------------------------------------------- |
| `asset.upload`       | `POST /assets` (uploading each transferred asset), `POST /assets/bulk-upload-check` (the dedup check) |
| `album.read`         | `GET /albums`, `GET /albums/{id}` (resolving `IMPORT_ALBUM`)      |
| `albumAsset.create`  | `PUT /albums/{id}/assets` (adding transferred assets to the target album) |

The `all` permission wildcard also works, and satisfies the check trivially. At startup the
program calls `GET /api-keys/me` and fails fast, listing exactly which of the three
permissions are missing, if any are.

## Docker Compose

There is no published registry image — build one locally with Nix and load it into your
local Docker daemon:

```sh
nix build .#docker
docker load < result
```

This produces an image tagged `ghcr.io/paulmiro/immich-federation-at-home:0.1.0`, built with
`dockerTools.buildLayeredImage` for the architecture of the machine you are on.

Both architectures can be built from either kind of machine, since the crate is
cross-compiled rather than emulated: `nix build .#docker-amd64` and `nix build
.#docker-arm64` name the target explicitly. `nix run .#docker-push` builds both, pushes each
under its own tag (`:0.1.0-amd64`, `:0.1.0-arm64`), and then publishes `:0.1.0` and `:latest`
as an OCI image index over the two, so a plain `docker pull` resolves to the right one.

```yaml
services:
  immich-federation-at-home:
    image: immich-federation-at-home:latest
    restart: unless-stopped
    environment:
      # Share link for the album on the *other* person's instance, and its password if any.
      EXPORT_ALBUM_URL: "https://photos.friend.example/share/AbC123"
      # EXPORT_ALBUM_PASSWORD: "..."

      # Your own instance and an API key with the permissions listed above.
      IMPORT_SERVER_URL: "https://immich.example.com"
      IMPORT_API_KEY: "..."
      # UUID or exact name of an *existing* album on your instance — it is never created.
      IMPORT_ALBUM: "Family Photos"

      # How often to check for new assets, and how many to transfer in parallel.
      IMPORT_INTERVAL: "1h"
      IMPORT_CONCURRENCY: "4"
      LOG_LEVEL: "info"
    tmpfs:
      # Each asset in flight is staged here in full before being re-uploaded, so this must
      # be sized for the single largest asset in the source album (a 4K video can be
      # several GB), not the album total. Omit `tmpfs:` entirely to let it fall back to
      # the container's writable layer on disk instead, if RAM is tight.
      - /tmp:size=8g
    volumes:
      # Strongly recommended if the source album has assets from an Immich external library
      # (see "How deduplication works" below). The image already sets CACHE_DIR, so the
      # cache exists either way — this only decides where it lives. Without this volume it
      # sits in the container's writable layer, which survives restarts but is thrown away
      # whenever the container is re-created (`down` then `up`, an image upgrade, any
      # compose change), costing a full re-download of every external-library asset each
      # time. A *named* volume, not a bind mount: the image creates CACHE_DIR owned by uid
      # 65532, the uid the container runs as, and a named volume inherits that ownership
      # automatically. A bind-mounted host directory is root-owned by default and will hit
      # a fatal startup error until you `chown 65532:65532` it yourself.
      - cache:/cache

volumes:
  cache:
```

## NixOS module

This flake exposes `nixosModules.default`. It runs the program the way the container does —
one long-lived process that sleeps `IMPORT_INTERVAL` between passes — under a systemd
`DynamicUser`, with the content-hash cache in `CacheDirectory` so it survives restarts and
reboots without any user or directory for you to create.

```nix
{
  inputs.immich-federation-at-home.url = "github:paulmiro/immich-federation-at-home";

  outputs = { nixpkgs, immich-federation-at-home, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        immich-federation-at-home.nixosModules.default
        {
          services.immich-federation-at-home = {
            enable = true;

            # Everything except the secrets, named exactly as in the table below. Anything
            # the program accepts works here, including variables newer than this module.
            settings = {
              EXPORT_ALBUM_URL = "https://photos.friend.example/share/AbC123";
              IMPORT_SERVER_URL = "https://immich.example.com";
              IMPORT_ALBUM = "Family Photos";
              IMPORT_INTERVAL = "1h";
            };

            # IMPORT_API_KEY=… and, if the share link has one, EXPORT_ALBUM_PASSWORD=….
            # A plain systemd EnvironmentFile, so anything that can drop one at activation
            # time (sops-nix, agenix, a file you chmod 600 yourself) fits.
            environmentFile = "/run/secrets/immich-federation-at-home.env";
          };
        }
      ];
    };
  };
}
```

Options: `enable`, `package` (defaults to this flake's build for the host's system),
`settings`, `environmentFile`.

`CACHE_DIR` is set by the module and does not belong in `settings` — the service gets
`CacheDirectory=immich-federation-at-home`, which under `DynamicUser` is really
`/var/cache/private/immich-federation-at-home` (root-owned, `0700`) reachable as
`/var/cache/immich-federation-at-home` from inside the unit. systemd re-chowns it to
whichever uid it allocates on the next start, so the cache keeps working across restarts
even though the user is different every time.

Keep secrets out of `settings`: it becomes `Environment=` lines in a unit file in the
world-readable Nix store. The module emits a build-time warning if it spots
`IMPORT_API_KEY` or `EXPORT_ALBUM_PASSWORD` there.

Assets are staged in `/tmp`, which `DynamicUser` makes private to the service. If `/tmp` is
a tmpfs too small for the largest asset in the album, set `TMPDIR` in `settings`.

## Running the binary directly

Three ways to get a binary, all verified against this repo:

* **`nix run .`** — builds (if needed) and runs the crane/Nix-built binary directly from a
  checkout, e.g. `nix run . -- --help`.
* **`nix build`** — produces `./result/bin/immich-federation-at-home`.
* **`cargo build --release`** — produces `target/release/immich-federation-at-home`
  (needs a Rust toolchain with a C linker on `PATH`; `nix develop` provides one — see
  [Development](#development)).

### systemd unit + timer

On NixOS use the [module](#nixos-module) instead. Elsewhere, for a `RUN_ONCE`-per-invocation
setup driven by systemd instead of the program's own built-in interval loop, run as a
dedicated non-root user:

```ini
# /etc/systemd/system/immich-federation-at-home.service
[Unit]
Description=Mirror a shared Immich album (one pass)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=immich-federation
Group=immich-federation
EnvironmentFile=/etc/immich-federation-at-home.env
Environment=RUN_ONCE=true
ExecStart=/usr/local/bin/immich-federation-at-home
```

```ini
# /etc/systemd/system/immich-federation-at-home.timer
[Unit]
Description=Run immich-federation-at-home hourly

[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
```

`/etc/immich-federation-at-home.env` holds `EXPORT_ALBUM_URL`, `IMPORT_SERVER_URL`,
`IMPORT_API_KEY`, `IMPORT_ALBUM`, etc.; keep it `chmod 600`, owned by
`immich-federation`. Note `RUN_ONCE` only accepts the literal strings `true`/`false` (not
`1`/`0` — that's a `clap` env-value constraint, not this program's own parsing).

## Environment variables

This table matches `src/config.rs` exactly (it is the single source of truth; run the
binary with `--help` to see the same information generated live from the same struct):

| Variable                | Required | Default | Meaning                                                                                          |
| ------------------------ | -------- | ------- | -------------------------------------------------------------------------------------------------- |
| `EXPORT_ALBUM_URL`      | yes      | —       | Share link for the album to mirror, e.g. `https://photos.friend.example/share/AbC123` or `.../s/holiday-2026`. Sub-path deployments and trailing slashes are tolerated. |
| `EXPORT_ALBUM_PASSWORD` | no       | unset   | Password for the share link above; leave unset if the link has none.                              |
| `IMPORT_SERVER_URL`     | yes      | —       | Base URL of your own instance, e.g. `https://immich.example.com`. A trailing `/` and a trailing `/api` are both tolerated and normalised away. |
| `IMPORT_API_KEY`        | yes      | —       | API key for the import instance; see [API key permissions](#api-key-permissions).                 |
| `IMPORT_ALBUM`          | yes      | —       | Target album on the import instance: a UUID, or an exact album name. It must already exist — this program errors out rather than creating it. |
| `IMPORT_INTERVAL`       | no       | `1h`    | How often to check for new assets, as a `humantime` duration (`30m`, `1h30m`, `6h`, …).            |
| `LOG_LEVEL`             | no       | `info`  | `error\|warn\|info\|debug\|trace`. **This is the only logging knob — `RUST_LOG` is not read.** The program has no `tracing`/`log`-facade dependency; see [Development](#development). |
| `IMPORT_CONCURRENCY`    | no       | `4`     | How many assets are downloaded/uploaded in parallel.                                              |
| `REQUEST_TIMEOUT`       | no       | `30s`   | Timeout for metadata calls (album listing, search pagination, permission checks, …).              |
| `TRANSFER_TIMEOUT`      | no       | `30m`   | Timeout for downloading and re-uploading a single asset.                                          |
| `RUN_ONCE`              | no       | `false` | Do one sync pass and exit instead of looping with `IMPORT_INTERVAL` between runs. Also settable via `--once`. Only `true`/`false` are accepted from the environment — not `1`/`0`. |
| `TMPDIR`                | no       | system  | Where assets are staged during transfer (read by the `tempfile` crate directly, not by this program's own code — see the Docker Compose `tmpfs` note above for sizing). |
| `CACHE_DIR`             | no       | unset, but **the container image sets it** to `/cache` and **the NixOS module sets it** to `/var/cache/immich-federation-at-home` | Directory for the content-hash cache (see [How deduplication works](#how-deduplication-works)). With no value at all — which in practice means running the binary directly, not the image — the cache is disabled entirely; nothing is lost, external-library assets are just re-downloaded every run. If set and the directory can't be created or written, the program exits at startup rather than failing later. Setting it to the empty string does **not** disable it: an empty environment variable is still a value, and the program then fails to create a directory with no name. |

Every flag has an equivalent `--kebab-case-flag`; a flag wins over its environment
variable if both are set (`--help` shows the full mapping).

## Setting up the share link

On the **export** (foreign) instance, share the album as a link (Album → Share → Create
link) and check:

* **Allow download** must be **on**. Without it every asset download fails; see
  [Troubleshooting](#troubleshooting) for what that looks like.
* **Show metadata** can be off — the sync doesn't rely on it. Immich's search API returns
  full asset metadata (checksum, filename) through the share-link API regardless of this
  setting.
* A password is optional; if set, pass it as `EXPORT_ALBUM_PASSWORD`.
* Note the link's expiry, if any — the program warns at startup if it expires within 7
  days, and every asset download will start failing once it does.

Copy the resulting URL (either the `/share/<key>` or the short `/s/<slug>` form) into
`EXPORT_ALBUM_URL`.

## Export instance version requirement

The export instance must be running **Immich v3.0.3 or newer**. Earlier versions either
can't enumerate an album's assets via the search API at all (pre-3.0.3) or need an
entirely different, unsupported asset-enumeration path (v2.x). The program checks
`GET /server/version` at startup and refuses to run against anything older, naming the
detected version in the error.

## How deduplication works

Every run re-lists the source album's assets (with their SHA-1 checksums) and asks the
import instance, via `POST /assets/bulk-upload-check`, "do you already have this
checksum?" Assets it already has are skipped; only the rest are downloaded and
re-uploaded, then all of them (fresh and pre-existing) are added to the target album,
which is itself idempotent.

This means the program keeps **no state it needs to be correct** — no database, and no
cache file is required for dedup to work. The only disk use step 3 needs is a temporary
file per asset while it's actively being transferred, so losing all on-disk data is
always trivially recoverable. The trade-off: if you permanently delete an imported asset
from your instance, dedup no longer sees its checksum, and the next run re-imports it.
Assets sitting in your instance's trash are detected and logged as already-present (not
re-uploaded), so trashing one has no such effect.

### External-library assets

There's one wrinkle to the checksum story above. If the source album contains an asset
that came from an Immich **external library** on the export instance (as opposed to a
normally-uploaded one), the "checksum" that instance reports for it isn't a hash of the
file's contents at all — Immich hashes external-library assets by their *path* on disk
(`sha1("path:" + originalPath)`) instead of opening the file, and the API gives us no way
to tell which kind of checksum we're looking at (`checksumAlgorithm` isn't exposed). Left
unhandled, that breaks dedup for exactly these assets in two ways: the `checksum` sent to
`bulk-upload-check` can never match anything on the import instance (which always
content-hashes on upload), and once downloaded, the bytes can never be verified against a
"checksum" that was never a hash of those bytes to begin with.

The program detects this positively — it recomputes the path hash itself and compares —
rather than guessing, so it can never misidentify an ordinary uploaded asset. The first
time it downloads such an asset it learns the real content hash and, if `CACHE_DIR` is
set, remembers it (keyed by the path hash, guarded by the file's modification time) so a
later run can dedup and verify it exactly like any other asset without downloading it
again. With no `CACHE_DIR` at all, these assets still transfer correctly — they're just
re-downloaded every run, since there's nowhere to remember the content hash between runs.
This only matters at all if the source album has external-library assets in it; ordinary
uploaded assets are unaffected either way.

Note that the container image sets `CACHE_DIR` itself, so **the cache is always on in the
container** — you only get the no-cache behaviour by running the binary directly with the
variable unset. What the compose volume decides is where that cache lives, and therefore
how long it survives: see the [Docker Compose](#docker-compose) snippet.

## Known limitations

* **Live photos**: the motion-video half is skipped, never uploaded as a standalone clip.
  Immich marks it `visibility: hidden` — both for a separately uploaded video (iPhone) and
  for one it extracted from an embedded motion photo (Pixel/Samsung `.MP.jpg`) — and hidden
  assets are excluded from the transfer. Embedded motion photos survive the round trip
  anyway, because the still's own bytes contain the video and the import instance extracts
  and re-links it itself; for a separately uploaded video the motion part is simply not
  transferred, and the still↔video pairing is not reconstructed on the import side.
* **v3.0.3+ only** on the export side (see above).
* **Additive only**: deletions and album removals on the source are never mirrored to the
  import side.
* **Deleted-here assets come back**: since dedup works by asking "do you have this
  checksum", permanently deleting an imported asset makes the next run re-import it.
  Trashed assets are detected and logged, not re-uploaded.
* **No sidecars**: XMP sidecar files on the source aren't fetched — the share API doesn't
  expose them. Embedded EXIF survives, since the original file bytes are uploaded
  untouched.
* **Album metadata** (description, sort order, cover photo) is not synced — only assets.
* **No corruption detection for external-library assets**: for a normal asset, the
  downloaded bytes are checked against the source checksum before upload. For an
  external-library asset with no cached content hash yet, that checksum is a path hash,
  not a content hash — there is nothing trustworthy to compare the download against, so a
  corrupted download can only be caught by a byte-count mismatch, not a hash mismatch.
* **A same-mtime edit to an external-library file goes unnoticed** by the cache. A cache
  entry is invalidated only when the file's modification time changes; an edit that
  happens to leave the mtime (and size) unchanged would serve a stale cached content hash
  instead of triggering a re-download. Out of scope by explicit decision.

## Secrets with secretspec

`secretspec.toml` is a declaration only — the binary itself just reads plain environment
variables and never parses this file. It exists so a developer (or an operator who'd
rather not put `IMPORT_API_KEY` in a `docker-compose.yaml` in plaintext) can run:

```sh
secretspec run -- immich-federation-at-home
```

and have `IMPORT_API_KEY` (required) and `EXPORT_ALBUM_PASSWORD` (optional) injected from
whatever provider is configured (keyring, 1Password, sops, …) — see
[secretspec.dev](https://secretspec.dev/).

## Troubleshooting

* **Download of an asset fails with a `400` error.** This is the "Allow download" setting
  being off on the share link, not a `401`/`403` as you might expect for a permissions
  problem — Immich's server-side access check for a disallowed share-link download throws
  a generic `BadRequestException` (400), not an auth error. Turn on "Allow download" on
  the share link (see [Setting up the share link](#setting-up-the-share-link)). The
  program's error message names this directly.
* **Startup fails listing missing permissions.** The `IMPORT_API_KEY` lacks one or more of
  `asset.upload`, `album.read`, `albumAsset.create`. Edit the key on the import instance
  and grant them, or use a key with the `all` wildcard.
* **`import album <id> does not exist` / `no album named "..." exists on the import
  instance`.** `IMPORT_ALBUM` is checked as a UUID first, then as an exact album name if it
  doesn't parse as one — a typo'd name, or one that doesn't match exactly (case,
  whitespace), falls through to the second error, which lists the albums that do exist. The
  album is never auto-created; create it on the import instance first.
  * If more than one album shares that exact name, startup fails instead with an
    "ambiguous name" error listing the candidate UUIDs — use a UUID in `IMPORT_ALBUM`
    instead.
* **Startup fails naming the export server's version.** The export instance is older than
  v3.0.3; see [Export instance version requirement](#export-instance-version-requirement).
  There is no workaround short of upgrading the export instance.
* **A wrong `EXPORT_ALBUM_PASSWORD` fails startup with a `401`.** A share link that turns
  out not to be password-protected at all logs a warning (not a failure) if a password was
  supplied anyway.
* **The share link has expired, or startup warns it will soon.** Ask whoever owns the
  export instance to extend or recreate the share link; `EXPORT_ALBUM_URL` (and the
  password, if it changes) will need updating either way.
* **An asset is logged as `unsupported format` and skipped.** The import instance's own
  upload validation rejected it (e.g. a file type it doesn't support). This is permanent —
  the program does not retry it on later runs.
* **Startup fails saying `CACHE_DIR` could not be created or written.** This is always a
  permissions problem, and it's fatal on purpose rather than a surprise later. Fix it one
  of three ways: use a named Docker volume instead of a bind mount (see
  [Docker Compose](#docker-compose) — it inherits the right ownership automatically);
  `chown 65532:65532` the bind-mounted host directory yourself, since that's the uid the
  container image runs as; or, if you are running the binary directly, unset `CACHE_DIR`
  to run without a cache. Removing `CACHE_DIR` from your compose file does *not* achieve
  the last one — the image sets it, so the value comes back. Point it somewhere writable
  and disposable like `/tmp` if you really want the container not to keep a cache.
* **The cache is empty again after every deploy.** Expected without a volume: `CACHE_DIR`
  then lives in the container's writable layer, which is discarded whenever the container
  is re-created. Add the named volume from the [Docker Compose](#docker-compose) snippet.

## Development

Everything below is verified to work in this repo as of this README:

* **`nix flake check`** — runs the full check suite: the package build, `cargo clippy
  --all-targets -- --deny warnings`, `cargo fmt --check`, and `cargo nextest` (which skips
  `#[ignore]`d tests by default, so the end-to-end suite below stays out of it).
* **`nix develop`** — a shell with `cargo`/`rustc`/`clippy`/`rustfmt`/`rust-analyzer`/`jq`/
  `curl`/`docker-compose` on `PATH`.
* **`nix run .#update-openapi`** — refreshes the vendored reference copy of the Immich
  OpenAPI spec at `openapi/immich-openapi-3.1.0.json` from
  `https://docs.immich.app/openapi.json`, and leaves the file untouched (reporting "no
  change") if nothing changed.
* **`tests/e2e/`** — a real two-instance end-to-end test against two `immich-server:v3.1.0`
  stacks. It is `#[ignore]`d so it never runs as part of `cargo test` or `nix flake check`,
  since it needs Docker. Run it for real with:

  ```sh
  docker compose -f tests/e2e/compose.yaml up -d --wait
  cargo test --test e2e -- --ignored --nocapture
  docker compose -f tests/e2e/compose.yaml down -v
  ```

  It signs up an admin on each instance, so it needs *fresh* stacks — always `down -v`
  between runs. Bringing both stacks up takes about a minute.

There is no `tracing`/`log`-facade dependency in this crate at all: logging is a
hand-written ~40-line module (`src/log.rs`) with one atomic level threshold. `RUST_LOG`
has no effect; `LOG_LEVEL` is the only knob.
