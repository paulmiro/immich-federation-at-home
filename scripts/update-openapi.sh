#!/usr/bin/env bash
# Refresh openapi/immich-openapi.json from upstream.
#
# The filename deliberately carries no version number: the version is `.info.version` inside
# the document, so an upstream release is a plain content change to a path that never moves,
# and none of the places that name the file (tests/spec_conformance.rs, the flake's source
# fileset, the README) have to be touched by hand.
#
# Source: https://docs.immich.app/openapi.json — verified by hand (see NOTES.md, "Task 14")
# to actually be the current release's OpenAPI document, not a guess. The immich-app/immich
# repo also carries this file under open-api/immich-openapi-specs.json, but that copy tracks
# `main` (ahead of the last release, with extra `x-immich-history` annotations) rather than
# the released version this project vendors against, so docs.immich.app is preferred.
#
# Requires: curl, jq, git (all provided via runtimeInputs by `nix run .#update-openapi`, or
# via `nix develop`; see flake.nix).

set -euo pipefail

UPSTREAM_URL="https://docs.immich.app/openapi.json"
SPEC_PATH="openapi/immich-openapi.json"

repo_root() {
    git rev-parse --show-toplevel
}

# Not `local` to main: the EXIT trap below runs after main has returned, by which point a
# local would be out of scope and `set -u` would abort the script on the way out.
tmp_file=""
cleanup() {
    if [ -n "$tmp_file" ]; then
        rm -f "$tmp_file"
    fi
}
trap cleanup EXIT

main() {
    local repo spec current_version new_version

    repo="$(repo_root)"
    spec="$repo/$SPEC_PATH"
    mkdir -p "$(dirname "$spec")"

    # Tolerate the file not being there at all, so a fresh checkout that somehow lost it can
    # get it back with one run.
    if [ -f "$spec" ]; then
        current_version="$(jq -r '.info.version' "$spec")"
    else
        current_version="(none vendored)"
    fi

    tmp_file="$(mktemp)"

    echo "fetching $UPSTREAM_URL ..."
    if ! curl -fsSL --max-time 30 -o "$tmp_file" "$UPSTREAM_URL"; then
        echo "error: failed to fetch $UPSTREAM_URL (network down, or URL moved)" >&2
        echo "the vendored spec at $spec was left untouched" >&2
        exit 1
    fi

    # Sanity-check we actually got an OpenAPI document, not an error page or redirect stub.
    if ! jq -e '.openapi and .info.version' "$tmp_file" >/dev/null 2>&1; then
        echo "error: fetched document doesn't look like an OpenAPI spec (no .openapi/.info.version)" >&2
        echo "the vendored spec at $spec was left untouched" >&2
        exit 1
    fi

    new_version="$(jq -r '.info.version' "$tmp_file")"

    if [ -f "$spec" ] && cmp -s "$tmp_file" "$spec"; then
        echo "no change: $current_version, byte-identical to $spec"
        exit 0
    fi

    mv "$tmp_file" "$spec"
    # mktemp makes the file 0600; the vendored spec is a normal committed source file.
    chmod 644 "$spec"

    if [ "$new_version" = "$current_version" ]; then
        echo "updated $spec (version unchanged: $current_version, content changed)"
    else
        echo "=================================================================="
        echo " UPSTREAM API VERSION CHANGED: $current_version -> $new_version"
        echo " Wrote $spec"
        echo " You should probably re-run the tests"
        echo "=================================================================="
    fi
}

main "$@"
