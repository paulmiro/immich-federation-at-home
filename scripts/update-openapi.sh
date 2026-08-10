#!/usr/bin/env bash
# Refresh openapi/immich-openapi-<version>.json from upstream.
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

repo_root() {
    git rev-parse --show-toplevel
}

main() {
    local repo openapi_dir current_file current_version tmp_file new_version new_file

    repo="$(repo_root)"
    openapi_dir="$repo/openapi"
    mkdir -p "$openapi_dir"

    # There should be exactly one vendored spec; tolerate zero (first run).
    current_file="$(find "$openapi_dir" -maxdepth 1 -name 'immich-openapi-*.json' -print -quit)"
    if [ -n "$current_file" ]; then
        current_version="$(jq -r '.info.version' "$current_file")"
    else
        current_version="(none vendored)"
    fi

    tmp_file="$(mktemp)"
    trap 'rm -f "$tmp_file"' EXIT

    echo "fetching $UPSTREAM_URL ..."
    if ! curl -fsSL --max-time 30 -o "$tmp_file" "$UPSTREAM_URL"; then
        echo "error: failed to fetch $UPSTREAM_URL (network down, or URL moved)" >&2
        echo "the vendored spec at ${current_file:-<none>} was left untouched" >&2
        exit 1
    fi

    # Sanity-check we actually got an OpenAPI document, not an error page or redirect stub.
    if ! jq -e '.openapi and .info.version' "$tmp_file" >/dev/null 2>&1; then
        echo "error: fetched document doesn't look like an OpenAPI spec (no .openapi/.info.version)" >&2
        echo "the vendored spec at ${current_file:-<none>} was left untouched" >&2
        exit 1
    fi

    new_version="$(jq -r '.info.version' "$tmp_file")"
    new_file="$openapi_dir/immich-openapi-$new_version.json"

    if [ -n "$current_file" ] && cmp -s "$tmp_file" "$current_file"; then
        echo "no change: $current_version, byte-identical to $current_file"
        exit 0
    fi

    if [ "$new_version" != "$current_version" ]; then
        echo "=================================================================="
        echo " UPSTREAM API VERSION CHANGED: $current_version -> $new_version"
        echo " Wrote $new_file"
        echo " This project hard-codes the old filename in several places —"
        echo " update them by hand before relying on this file:"
        echo "   - tests/spec_conformance.rs (the vendored-spec path)"
        echo "   - PLAN.md section 3 / section 11 (mentions of the filename)"
        echo "   - flake.nix's source fileset, if it names the file explicitly"
        echo "=================================================================="
        mv "$tmp_file" "$new_file"
        # mktemp makes the file 0600; the vendored spec is a normal committed source file.
        chmod 644 "$new_file"
        if [ -n "$current_file" ]; then
            rm -f "$current_file"
            echo "removed old $current_file"
        fi
    else
        echo "updated $current_file (version unchanged: $current_version, content changed)"
        mv "$tmp_file" "$current_file"
        chmod 644 "$current_file"
    fi
}

main "$@"
