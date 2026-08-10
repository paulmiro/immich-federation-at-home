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
3. [Running the binary directly](#running-the-binary-directly)
4. [Environment variables](#environment-variables)
5. [Setting up the share link](#setting-up-the-share-link)
6. [Export instance version requirement](#export-instance-version-requirement)
7. [How deduplication works](#how-deduplication-works)
8. [Known limitations](#known-limitations)
9. [Secrets with secretspec](#secrets-with-secretspec)
10. [Troubleshooting](#troubleshooting)
11. [Development](#development)

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

This produces an image tagged `immich-federation-at-home:latest`. It's built with
`dockerTools.buildLayeredImage` for `x86_64-linux`; cross-building for `aarch64-linux` is
out of scope (build it *on* the aarch64 machine and it'll produce a native image there).

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
    # No volumes: the program is fully stateless (see "How deduplication works" below).
```

## Running the binary directly

Three ways to get a binary, all verified against this repo:

* **`nix run .`** — builds (if needed) and runs the crane/Nix-built binary directly from a
  checkout, e.g. `nix run . -- --help`.
* **`nix build`** — produces `./result/bin/immich-federation-at-home`.
* **`cargo build --release`** — produces `target/release/immich-federation-at-home`
  (needs a Rust toolchain with a C linker on `PATH`; `nix develop` provides one — see
  [Development](#development)).

### systemd unit + timer

For a `RUN_ONCE=1`-per-invocation setup driven by systemd instead of the program's own
built-in interval loop, run as a dedicated non-root user:

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

This means the program keeps **no state of its own** — no database, no cache file. The
only disk use is a temporary file per asset while it's actively being transferred, so
losing all on-disk data is always trivially recoverable (there is none to lose). The
trade-off: if you permanently delete an imported asset from your instance, dedup no
longer sees its checksum, and the next run re-imports it. Assets sitting in your instance's
trash are detected and logged as already-present (not re-uploaded), so trashing one has
no such effect.

## Known limitations

* **Live photos**: the still and motion-video parts are separate assets in Immich. If the
  share link exposes both, each gets uploaded as its own asset — the pairing between them
  is not reconstructed on the import side.
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
* **`tests/e2e/`** exists — a real two-instance Docker Compose end-to-end test — but per
  its own header comment it has been **written and never executed**, not even once, not
  even manually. It compiles, lints, and is correctly `#[ignore]`d so it never runs as
  part of `cargo test` or `nix flake check`; running it for real (`cargo test --test e2e
  -- --ignored`, with `tests/e2e/compose.yaml` up) is unverified territory. Treat it as a
  best-effort transcription of the API, not proven-correct code.

There is no `tracing`/`log`-facade dependency in this crate at all: logging is a
hand-written ~40-line module (`src/log.rs`) with one atomic level threshold. `RUST_LOG`
has no effect; `LOG_LEVEL` is the only knob.
